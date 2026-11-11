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
//! beside a foreign loader. The host's loader never runs as installed:
//! it would still search its compiled-in default directories, where any
//! host library is found. A private copy closed to those directories by
//! [`crate::ldso::scrub_glibc_loader`] runs instead, and a host loader
//! that cannot be closed leaves the bundled pair in place.
//!
//! Winners from the host are materialized as symlinks in a link farm, and
//! the runtime shows each one the bundle lacks in the bundle's library
//! directory. No host directory is ever searched, so a soname the bundle
//! lacks and the resolver did not choose fails by name instead of being
//! satisfied by whatever the host has.
//!
//! A launch that activates only part of the package's dependency sets
//! records its decision under the canonical identity of the pins it
//! activated, so two selections never serve each other's decisions while
//! lists that differ only in order share one.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::drivers::{self, DRIVER_FAMILIES};
use crate::footer::HostLibsPolicy;
use crate::ldso::{self, MAX_LOADER_BYTES};
use crate::plugindirs::PluginDir;
use crate::verdef::{self, Choice, ElfFacts, LibcFamily, VersionSet};

/// What a launch takes from the host.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Resolution {
    /// Directory of symlinks to the chosen host libraries, when any were
    /// chosen.
    pub farm: Option<PathBuf>,
    /// The closed copy of the host loader to run the entrypoint under,
    /// when the host's libc won. The bundled loader is used otherwise.
    pub host_interp: Option<HostLoader>,
    /// Sonames whose two copies could not be ordered. The bundled copy is
    /// used; these are reported so the publisher can see them.
    pub incomparable: Vec<String>,
    /// The host modules the counterpart directories hold that the
    /// decision admitted, as their host paths, sorted. A launch is
    /// meant to present them beside the bundled members of their plugin
    /// directory; their host dependencies come through the farm.
    pub admitted: Vec<PathBuf>,
    /// The merged view of each plugin directory with admitted modules,
    /// sorted by the directory's package path. A view links the bundled
    /// members first and the admitted host members whose names the
    /// package does not carry after them, so wherever the directory is
    /// redirected the launch presents that order. Built only by
    /// [`resolve_selected`]; a view that cannot be built is left out and
    /// its directory keeps the bundled members alone.
    pub plugin_views: Vec<PluginView>,
}

/// One plugin directory's merged view, built in the decision's store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginView {
    /// The plugin directory's package path, relative to the package root.
    pub relative: String,
    /// The view directory. Every entry is a symlink: one per bundled
    /// member, aimed at the package's copy, then one per admitted host
    /// member whose name no bundled member holds, aimed at the host's.
    pub dir: PathBuf,
}

/// The private copy of the host loader a launch runs, closed to the
/// loader's default search directories, and the digest it was written
/// with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostLoader {
    /// The copy, under the uid's cache root.
    pub path: PathBuf,
    /// BLAKE3 of the copy's bytes.
    pub hash: blake3::Hash,
}

impl HostLoader {
    /// Check the copy against its digest. A recorded decision is served
    /// and a kept copy reused only after this passes, so a copy damaged or
    /// replaced since it was written is never handed to a launch.
    pub fn verify(&self) -> io::Result<()> {
        let image = read_loader(&self.path)?;
        if blake3::hash(&image) == self.hash {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the copy does not match the digest it was written with",
            ))
        }
    }
}

/// A host loader the resolver chose, closed in memory: where it came
/// from, where its copy goes, and the closed image with its digest. The
/// image is the one the copy is written from, so the host's loader is
/// read once per decision.
#[derive(Clone, PartialEq, Eq)]
pub struct ClosedLoader {
    /// The host's loader.
    pub source: PathBuf,
    /// Where the closed copy is kept.
    pub copy: PathBuf,
    /// BLAKE3 of the closed image.
    pub hash: blake3::Hash,
    /// The closed image.
    pub image: Vec<u8>,
}

impl std::fmt::Debug for ClosedLoader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClosedLoader")
            .field("source", &self.source)
            .field("copy", &self.copy)
            .field("hash", &self.hash)
            .field("image_len", &self.image.len())
            .finish()
    }
}

/// One launch's inputs.
pub struct Request<'a> {
    /// The package root as the entrypoint will see it.
    pub pkg_root: &'a Path,
    /// Library directories relative to `pkg_root`.
    pub lib_dirs: &'a [&'a str],
    /// The plugin directories the package carries, from its
    /// `.onelf/plugin-dirs` record. Their host counterparts join the
    /// decision fingerprint, so installing or removing a host module
    /// recomputes the decision. A package without the record has none.
    pub plugin_dirs: &'a [PluginDir],
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
    /// A fetched GL build's extracted root, indexed ahead of the host:
    /// its [`GL_BUILD_LIB_DIRS`] and [`GL_BUILD_ICD_DIRS`] supply what
    /// the host could not.
    pub extra_root: Option<&'a Path>,
    /// Extracted roots of the dependency sets the package pins. Their
    /// libraries are the ones the bundle left out, so they are compared
    /// against the host exactly as a bundled copy would be.
    pub set_roots: &'a [PathBuf],
    /// Whether the launch can place host files over their bundled copies.
    /// Without that the host's glibc is never chosen: the bundled
    /// executables' run path would still find the bundled libc, and it
    /// cannot run under the host loader.
    pub host_libc: bool,
    /// Where a closed host loader's copy is kept, as the runtime judged
    /// it. Without a usable one the host's glibc is never chosen.
    pub loader_store: LoaderStore<'a>,
    /// The host's name service configuration. A host libc opens the
    /// modules it names by soname. [`nsswitch_file`] outside of tests.
    pub nsswitch: &'a Path,
}

/// Where hosts keep their name service configuration, in the order the
/// host libc looks: an administrator's file, then the distribution
/// default openSUSE ships under `/usr/etc`.
const NSSWITCH_FILES: &[&str] = &["/etc/nsswitch.conf", "/usr/etc/nsswitch.conf"];

/// The host's name service configuration file, the first of
/// [`NSSWITCH_FILES`] that exists.
pub fn nsswitch_file() -> &'static Path {
    NSSWITCH_FILES
        .iter()
        .map(Path::new)
        .find(|path| path.exists())
        .unwrap_or(Path::new(NSSWITCH_FILES[0]))
}

/// Where closed host loader copies go, or why none can be kept.
///
/// A copy is executed, so it has to live on a filesystem that allows it.
/// The runtime checks that before it builds a request, since it alone can
/// ask the kernel how a filesystem is mounted.
#[derive(Debug, Clone, Copy)]
pub enum LoaderStore<'a> {
    /// The uid's cache root. Copies go under its `loaders/`, content
    /// addressed, so one copy serves every package and decision.
    Root {
        /// The cache root.
        root: &'a Path,
        /// The uid that has to own every directory on the way.
        uid: u32,
    },
    /// No copy can be kept, and why.
    Refused(&'a str),
}

/// Library directories of a GL build, relative to its root.
pub const GL_BUILD_LIB_DIRS: &[&str] = &["lib", "lib64"];

/// Driver description directories of a GL build, relative to its root.
pub const GL_BUILD_ICD_DIRS: &[&str] = &["share/vulkan/icd.d", "share/glvnd/egl_vendor.d"];

/// Libraries a GL build carries that nothing names in `DT_NEEDED`: the
/// glvnd vendor libraries, Mesa's gallium and DRI megadrivers, Vulkan
/// ICDs and layers are all opened by name at runtime. They seed the
/// closure walk so they and their dependencies reach the farm, since
/// the build's directories are never put on the search path themselves.
const GL_BUILD_SEEDS: &[&str] = &[
    "libGLX_",
    "libEGL_",
    "libgallium",
    "libdril",
    "libvulkan_",
    "libVkLayer_",
];

/// Host libraries that glvnd opens by a name built from the X server's
/// vendor string, so neither `DT_NEEDED` nor any driver description file
/// names them. They seed the host's closure walk like the driver
/// families, or a package reaching GL through GLX cannot load the
/// vendor the host's display names.
const HOST_SEEDS: &[&str] = &["libGLX_"];

/// Where a launch gets its GL stack from, if anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gl {
    Bundled,
    Host,
    /// The host lacks an entry point the bundle links, so the bundle
    /// fails without one.
    Missing,
    /// Nothing links an entry point and the host has none. A library may
    /// still open one by name, as libepoxy and SDL do, but only when the
    /// application draws with GL.
    Absent,
}

impl Gl {
    /// Whether neither the bundle nor the host has a GL stack, so a
    /// pinned build is worth fetching.
    pub fn lacking(self) -> bool {
        matches!(self, Gl::Missing | Gl::Absent)
    }
}

/// The entry points of a GL or Vulkan stack. One of them present, on
/// either side, is a stack the application can load.
const GL_ENTRY: &[&str] = &["libGL.so", "libEGL.so", "libGLESv2.so", "libvulkan.so"];

fn has_gl_entry(libs: &BTreeMap<String, PathBuf>) -> bool {
    libs.keys()
        .any(|name| GL_ENTRY.iter().any(|p| name.starts_with(p)))
}

/// The GL entry points `carried` objects and `executable` name and
/// `carried` does not itself hold. This is what the launch will ask the
/// host, or a pinned build, to supply.
fn gl_entries_wanted(
    carried: &BTreeMap<String, PathBuf>,
    executable: Option<&Path>,
) -> BTreeSet<String> {
    let mut wanted = BTreeSet::new();
    for path in carried.values().map(PathBuf::as_path).chain(executable) {
        for needed in verdef::read_needed(path).unwrap_or_default() {
            if GL_ENTRY.iter().any(|p| needed.starts_with(p)) && !carried.contains_key(&needed) {
                wanted.insert(needed);
            }
        }
    }
    wanted
}

/// Whether the bundle or the host carries a GL stack. Decided before a
/// pinned build is fetched, so the host is indexed without one.
///
/// `extra_dirs` are library directories beside the package, which is
/// where a shared dependency set puts a toolkit, and a toolkit is
/// usually what names a GL entry point. `executable` is the binary the
/// launch runs, which links GL itself as often as its libraries do.
///
/// A host counts only when it has every entry point the package asks
/// for. Some hosts have part of a stack and not the rest: an image with
/// `libGL.so.1` and no `libEGL.so.1` is one an X server pulls in, and
/// treating that as a stack leaves the application to fail on the
/// missing half.
pub fn gl_situation(
    pkg_root: &Path,
    lib_dirs: &[&str],
    extra_dirs: &[PathBuf],
    executable: Option<&Path>,
    ld_cache: &Path,
    icd_dirs: &[&str],
) -> Gl {
    let mut carried = bundled_libs(pkg_root, lib_dirs);
    for dir in extra_dirs {
        for (name, path) in libs_in(dir) {
            carried.entry(name).or_insert(path);
        }
    }
    if has_gl_entry(&carried) {
        return Gl::Bundled;
    }
    let host = HostIndex::load(ld_cache, icd_dirs, None).libs;
    let wanted = gl_entries_wanted(&carried, executable);
    // Nothing names one outright, so a plugin may still ask for one by
    // dlopen and any entry point is worth having.
    if wanted.is_empty() {
        return if has_gl_entry(&host) {
            Gl::Host
        } else {
            Gl::Absent
        };
    }
    if wanted.iter().all(|name| host.contains_key(name)) {
        Gl::Host
    } else {
        Gl::Missing
    }
}

const LIBC: &str = "libc.so.6";

/// Everything that must come from the same build as the loader, matched
/// as soname prefixes. The loader itself is recognized by
/// [`ldso::is_glibc_loader_name`].
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

/// Whether `soname` belongs to the glibc family, which a package always
/// carries as a unit with its loader.
pub fn is_libc_family(soname: &str) -> bool {
    LIBC_FAMILY.iter().any(|p| soname.starts_with(p)) || ldso::is_glibc_loader_name(soname)
}

fn is_driver_family(soname: &str) -> bool {
    DRIVER_FAMILIES.iter().any(|p| soname.starts_with(p))
}

/// Decide what this launch takes from the host, for a launch whose
/// active dependency sets are `pins`, empty for a launch that activates
/// none.
///
/// The decision is read and written under the launch's
/// [`SelectionIdentity`] as well as the host: a changed subset or pin
/// misses and is recomputed against only its own library universe, an
/// empty selection never reads a toolkit selection's record, and an
/// equivalent list order hits and is served as it stands. A list outside
/// the identity's bounds decides afresh every launch rather than being
/// cached under a guessed key. A host change, a winner swapped in place
/// and a vanished winner each void a record, as does a record left by a
/// release that keyed decisions on the host alone.
pub fn resolve_selected(req: &Request, pins: &[ActivePin]) -> Resolution {
    if req.policy == HostLibsPolicy::Never {
        return Resolution::default();
    }
    let host = HostIndex::load(req.ld_cache, req.icd_dirs, req.extra_root);
    if host.libs.is_empty() {
        return Resolution::default();
    }
    let fingerprint = fingerprint(
        req.ld_cache,
        req.plugin_dirs,
        req.icd_dirs,
        req.policy,
        req.extra_root,
        req.host_libc,
        req.nsswitch,
        req.loader_store,
    );
    let Some(identity) = SelectionIdentity::new(pins) else {
        return with_plugin_views(fresh(req, &fingerprint, &host).1, req, &fingerprint);
    };
    let Some(dir) = selection_dir(identity.key()).map(|name| req.store.join(name)) else {
        return with_plugin_views(fresh(req, &fingerprint, &host).1, req, &fingerprint);
    };
    if let Some(recorded) =
        load_selection(&dir, &identity, &fingerprint, req.store, req.loader_store)
    {
        return with_plugin_views(recorded, req, &fingerprint);
    }
    let (decision, resolution, recordable) = fresh(req, &fingerprint, &host);
    if recordable
        && let Err(e) = record_selection(
            &dir,
            &identity,
            &fingerprint,
            &resolution,
            &decision.winners,
        )
    {
        eprintln!(
            "onelf-rt: resolver: cannot record {}: {e}",
            req.store.display()
        );
    }
    with_plugin_views(resolution, req, &fingerprint)
}

/// A fresh decision for `req` with its farm and loader copy built, and
/// whether it may be recorded.
///
/// When the closed loader copy cannot be kept after all, a race or a full
/// disk past what the runtime could judge up front, the request is
/// decided again with the store refused. The bundled pair runs and the
/// rest of the decision stands, the driver closure with it. That fallback
/// is not recorded, so a later launch tries the copy again. A resolution
/// whose farm could not be built is likewise not recorded, so a later
/// launch tries the farm again.
fn fresh(req: &Request, fingerprint: &str, host: &HostIndex) -> (Decision, Resolution, bool) {
    let decision = decide_on(req, host);
    let Some(loader) = &decision.host_interp else {
        let (resolution, farm_built) = materialized(req, fingerprint, &decision);
        return (decision, resolution, farm_built);
    };
    match keep_loader_copy(loader, req.loader_store) {
        Ok(()) => {
            let (resolution, farm_built) = materialized(req, fingerprint, &decision);
            (decision, resolution, farm_built)
        }
        Err(why) => {
            eprintln!(
                "onelf-rt: resolver: the host loader {} is not used, {why}; the bundled loader \
                 and libc run",
                loader.source.display()
            );
            let refused = Request {
                loader_store: LoaderStore::Refused("the loader copy cannot be kept"),
                ..*req
            };
            let decision = decide_on(&refused, host);
            let (resolution, _) = materialized(&refused, fingerprint, &decision);
            (decision, resolution, false)
        }
    }
}

/// The resolution of a decision, with its farm built, and whether the
/// farm was built. A farm that cannot be written leaves nothing taken
/// from the host, and the caller leaves the record alone so a later
/// launch tries again.
fn materialized(req: &Request, fingerprint: &str, decision: &Decision) -> (Resolution, bool) {
    let copy = decision.host_interp.as_ref().map(|l| l.copy.as_path());
    let farm = match materialize(req.store, fingerprint, &decision.winners, copy) {
        Ok(farm) => farm,
        Err(e) => {
            eprintln!(
                "onelf-rt: resolver: cannot write {}: {e}",
                req.store.display()
            );
            return (Resolution::default(), false);
        }
    };
    let resolution = Resolution {
        farm,
        host_interp: decision.host_interp.as_ref().map(|loader| HostLoader {
            path: loader.copy.clone(),
            hash: loader.hash,
        }),
        incomparable: decision.incomparable.clone(),
        admitted: decision.admitted.clone(),
        plugin_views: Vec::new(),
    };
    (resolution, true)
}

/// A host module the resolver refused, with the first object of its
/// closure that failed a check and why it failed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ModuleRefusal {
    /// The module refused.
    pub module: PathBuf,
    /// The first object of the module's closure that failed a check,
    /// the module itself when its own facts fail.
    pub object: PathBuf,
    /// Why that object failed.
    pub reason: String,
}

/// What the resolver decides for a request, worked out afresh and with
/// nothing written.
#[derive(Debug, Default)]
pub struct Decision {
    /// Every soname weighed, in order, with the copy that won: `None` for
    /// one the bundle lacks, which only the host can supply.
    pub compared: Vec<(String, Option<Choice>)>,
    /// The host libraries chosen, by soname. A loader soname names the
    /// interpreter the pair runs, not the cache's entry for it.
    pub winners: BTreeMap<String, PathBuf>,
    /// The host loader the pair runs under, when the host's libc won and
    /// its loader could be closed.
    pub host_interp: Option<ClosedLoader>,
    /// The host loader of a winning host libc that could not be closed,
    /// and why. The bundled pair runs instead.
    pub host_interp_refused: Option<(PathBuf, String)>,
    pub incomparable: Vec<String>,
    /// The host modules the counterpart directories hold that the
    /// decision admitted, sorted by path. Their host dependencies are
    /// among the winners.
    pub admitted: Vec<PathBuf>,
    /// The host modules the decision refused, sorted by module path.
    pub refused: Vec<ModuleRefusal>,
    /// Where in the store the link farm for these winners lives.
    pub farm: Option<PathBuf>,
}

/// [`resolve_selected`] without the store: neither the recorded decision
/// is read nor a farm or record written. For reporting what a launch would do.
pub fn decide(req: &Request) -> Decision {
    if req.policy == HostLibsPolicy::Never {
        return Decision::default();
    }
    let host = HostIndex::load(req.ld_cache, req.icd_dirs, req.extra_root);
    if host.libs.is_empty() {
        return Decision::default();
    }
    let mut decision = decide_on(req, &host);
    if !decision.winners.is_empty() {
        let fingerprint = fingerprint(
            req.ld_cache,
            req.plugin_dirs,
            req.icd_dirs,
            req.policy,
            req.extra_root,
            req.host_libc,
            req.nsswitch,
            req.loader_store,
        );
        let copy = decision.host_interp.as_ref().map(|l| l.copy.as_path());
        decision.farm = Some(farm_dir(req.store, &fingerprint, &decision.winners, copy));
    }
    decision
}

fn decide_on(req: &Request, host: &HostIndex) -> Decision {
    let mut bundled = bundled_libs(req.pkg_root, req.lib_dirs);
    // A name the bundle carries itself wins: the set supplies what was
    // left out, not a second copy of what was kept.
    for root in req.set_roots {
        for (name, path) in bundled_libs(root, GL_BUILD_LIB_DIRS) {
            bundled.entry(name).or_insert(path);
        }
    }
    let mut decision = Decision::default();

    if let (true, Some(bundled_libc), Some(host_libc)) =
        (req.host_libc, bundled.get(LIBC), host.libs.get(LIBC))
    {
        let choice = verdef::compare(&versions(bundled_libc), &versions(host_libc));
        decision.compared.push((LIBC.to_string(), Some(choice)));
        match choice {
            Choice::Host => {
                if let Some(interp) = host.loader_for(host_libc) {
                    let closed = match req.loader_store {
                        LoaderStore::Root { root, .. } => close_loader(&interp).map(|image| {
                            let hash = blake3::hash(&image);
                            (loader_copy(root, &hash, &loader_name(&interp)), hash, image)
                        }),
                        LoaderStore::Refused(why) => Err(why.to_string()),
                    };
                    match closed {
                        Ok((copy, hash, image)) => {
                            for name in bundled.keys().filter(|n| is_libc_family(n)) {
                                let path = if ldso::is_glibc_loader_name(name) {
                                    Some(interp.clone())
                                } else {
                                    host.libs.get(name).cloned()
                                };
                                if let Some(path) = path {
                                    decision.winners.insert(name.clone(), path);
                                }
                            }
                            decision.host_interp = Some(ClosedLoader {
                                source: interp,
                                copy,
                                hash,
                                image,
                            });
                        }
                        Err(why) => decision.host_interp_refused = Some((interp, why)),
                    }
                }
            }
            Choice::Incomparable => decision.incomparable.push(LIBC.to_string()),
            Choice::Bundle => {}
        }
    }

    // What the host is expected to supply is chosen first, so the
    // counterpart modules below weigh themselves against the copies the
    // decision actually installs. A member of the driver closure the
    // bundle also carries is compared; one it does not carry can only
    // come from the host. Under `always` every bundled soname is
    // compared as well.
    let mut candidates: Vec<String> = host.driver_closure().into_iter().collect();
    if decision.host_interp.is_some() {
        candidates.extend(host.closure(host_libc_opens(req.nsswitch)));
    }
    if req.policy == HostLibsPolicy::Always {
        candidates.extend(bundled.keys().cloned());
    }
    candidates.sort();
    candidates.dedup();
    let mut chosen: BTreeSet<String> = BTreeSet::new();
    let mut choose = |decision: &mut Decision, name: &str| {
        if !chosen.insert(name.to_string()) {
            return;
        }
        if is_libc_family(name) {
            // A closed host loader finds nothing on its own, so a member
            // of its family that the host libc or a host driver opens and
            // the bundle lacks has to be chosen like any other library.
            // A glibc loader soname instead names the interpreter the
            // pair runs, whatever the cache's entry carries: the farm
            // links the closed copy under the demanded name.
            if let Some(interp) = decision.host_interp.as_ref() {
                if ldso::is_glibc_loader_name(name) {
                    decision
                        .winners
                        .entry(name.to_string())
                        .or_insert(interp.source.clone());
                } else if !bundled.contains_key(name)
                    && let Some(host_path) = host.libs.get(name)
                {
                    decision.compared.push((name.to_string(), None));
                    decision.winners.insert(name.to_string(), host_path.clone());
                }
            }
            return;
        }
        let Some(host_path) = host.libs.get(name) else {
            return;
        };
        let choice = bundled
            .get(name)
            .map(|bundled_path| verdef::compare(&versions(bundled_path), &versions(host_path)));
        decision.compared.push((name.to_string(), choice));
        match choice.unwrap_or(Choice::Host) {
            Choice::Host => {
                decision.winners.insert(name.to_string(), host_path.clone());
            }
            Choice::Incomparable => decision.incomparable.push(name.to_string()),
            Choice::Bundle => {}
        }
    };
    for name in &candidates {
        choose(&mut decision, name);
    }

    // The package's counterpart modules lean on the copies the launch
    // would load, winners included; the refusals ride the decision for
    // the doctor to report. The sonames the admitted closures reached
    // are then chosen like any other, so they and their needs reach the
    // farm.
    let counterpart = counterpart_dirs(req.plugin_dirs, req.ld_cache);
    if !counterpart.is_empty() {
        let loader = decision.host_interp.as_ref().map(|l| l.source.as_path());
        let (admitted, refused, reached) = admit_modules(
            host,
            &bundled,
            &decision.winners,
            &counterpart,
            bundled.get(LIBC).map(|p| p.as_path()),
            loader,
        );
        decision.admitted = admitted;
        decision.refused = refused;
        for name in &reached {
            choose(&mut decision, name);
        }
    }
    decision
}

/// One active dependency set of a launch: the set's canonical
/// declaration name and the full hash its build is pinned by.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ActivePin {
    /// The declaration name, as the package's set table names it.
    pub label: String,
    /// The pinned build's BLAKE3 digest in hex.
    pub hash: String,
}

/// The canonical identity of a launch's active pin set.
///
/// Built by sorting the pins by declaration name and hash, collapsing
/// exact duplicates and normalizing every hash to lowercase, so lists
/// that differ only in order or in hash case give one identity and any
/// difference in membership, name or pin gives two. The identity is
/// bounded and total: a name over the label bound, a name or hash
/// carrying a control character, a hash that is not one digest, or more
/// than the pin-count bound refuses to build, and a launch it cannot
/// identify is decided afresh rather than cached under a guessed key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionIdentity {
    pins: Vec<ActivePin>,
    canonical: String,
    key: String,
}

impl SelectionIdentity {
    /// The identity of `pins`, or `None` when a pin is outside the
    /// bounds.
    pub fn new(pins: &[ActivePin]) -> Option<Self> {
        if pins.len() > MAX_ACTIVE_PINS {
            return None;
        }
        let mut pins: Vec<ActivePin> = pins.to_vec();
        for pin in &mut pins {
            pin.hash = pin.hash.to_ascii_lowercase();
        }
        pins.sort();
        pins.dedup();
        let mut canonical = String::new();
        for pin in &pins {
            pin_in_bounds(pin)?;
            if !canonical.is_empty() {
                canonical.push('\n');
            }
            canonical.push_str(&pin_line(pin));
        }
        let digest = blake3::hash(canonical.as_bytes());
        let key = hex_prefix(&digest.as_bytes()[..SELECTION_KEY_BYTES]);
        Some(Self {
            pins,
            canonical,
            key,
        })
    }

    /// The pins in their canonical order.
    pub fn pins(&self) -> &[ActivePin] {
        &self.pins
    }

    /// The canonical text the identity is: each pin's record line, in
    /// canonical order, newline separated, and empty for an empty
    /// selection. This is the exact text a record stores and the key
    /// digests.
    pub fn canonical(&self) -> &str {
        &self.canonical
    }

    /// The bounded store name the identity is keyed by: lowercase hex
    /// over a digest of [`Self::canonical`]. Equal identities share it.
    /// A record still names the full canonical pins and a reader
    /// compares them whole, so the key only has to find the record,
    /// never to vouch for it.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The record lines the canonical text is made of.
    fn identity_lines(&self) -> Vec<&str> {
        self.canonical.lines().collect()
    }
}

/// Whether `pin` is inside the bounds a record can carry: a nonempty
/// name of at most the label bound, with no control character, and
/// exactly one BLAKE3 digest in lowercase hex.
fn pin_in_bounds(pin: &ActivePin) -> Option<()> {
    let label = pin.label.as_bytes();
    if label.is_empty() || label.len() > MAX_PIN_LABEL_BYTES {
        return None;
    }
    if label.iter().any(|b| *b < 0x20 || *b == 0x7f) {
        return None;
    }
    if pin.hash.len() != 64
        || !pin
            .hash
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    Some(())
}

/// One pin's canonical record line, `pin <name length>:<name>\t<hash>`.
/// The length prefix and the admitted names make the line injective, so
/// two pins never render one line and a record can be compared as whole
/// lines.
fn pin_line(pin: &ActivePin) -> String {
    format!("pin {}:{}\t{}", pin.label.len(), pin.label, pin.hash)
}

/// Lowercase hex of `bytes`.
fn hex_prefix(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The store subdirectory for one selection identity key, `sel-<key>`,
/// or `None` when `key` is not a key [`SelectionIdentity`] mints: the
/// name is always one bounded lowercase hex component, so nothing a
/// caller passes can name anything outside the store.
pub fn selection_dir(key: &str) -> Option<String> {
    let hex = !key.is_empty()
        && key.len() <= SELECTION_KEY_BYTES * 2
        && key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    hex.then(|| format!("{SELECTION_DIR_PREFIX}{key}"))
}

/// A library that cannot be read is treated as defining nothing: an
/// unreadable host library loses to the bundled copy, and an unreadable
/// bundled library loses to the host copy.
fn versions(path: &Path) -> VersionSet {
    verdef::read(path).unwrap_or_default()
}

/// The host's libraries by soname, from its loader cache and the drivers
/// its ICD files name by path.
struct HostIndex {
    libs: BTreeMap<String, PathBuf>,
    /// Drivers reached through ICD files rather than the cache, and a GL
    /// build's dlopen-only libraries. They seed the closure walk alongside
    /// the driver families.
    icd_libs: Vec<PathBuf>,
    /// What the host's driver modules need. Mesa and libva open those
    /// modules by absolute path, so the modules load without the farm but
    /// their dependencies are looked up by soname and must be in it.
    module_needs: BTreeSet<String>,
}

impl HostIndex {
    /// The extra root goes in first, so a soname it carries shadows the
    /// host's copy and its drivers seed the closure walk.
    fn load(ld_cache: &Path, icd_dirs: &[&str], extra_root: Option<&Path>) -> Self {
        let mut libs = BTreeMap::new();
        let mut icd_libs = Vec::new();
        if let Some(root) = extra_root {
            libs.extend(bundled_libs(root, GL_BUILD_LIB_DIRS));
            icd_libs.extend(
                libs.iter()
                    .filter(|(name, _)| GL_BUILD_SEEDS.iter().any(|p| name.starts_with(p)))
                    .map(|(_, path)| path.clone()),
            );
            let dirs: Vec<String> = GL_BUILD_ICD_DIRS
                .iter()
                .map(|d| root.join(d).to_string_lossy().into_owned())
                .collect();
            let dirs: Vec<&str> = dirs.iter().map(String::as_str).collect();
            // The build's ICD files name their drivers by bare soname, so
            // they relocate; those resolve against the build's own
            // directories rather than a cache.
            icd_libs.extend(icd_library_paths_in(&dirs, Some(&libs)));
        }
        for (soname, path) in drivers::cache_paths_in(ld_cache) {
            libs.entry(soname).or_insert_with(|| PathBuf::from(path));
        }
        // After the cache, so an ICD file naming its driver by bare soname
        // (NVIDIA's `libGLX_nvidia.so.0`, which no driver family names)
        // resolves through it and seeds the walk like one named by path.
        icd_libs.extend(icd_library_paths_in(icd_dirs, Some(&libs)));
        for path in &icd_libs {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                libs.entry(name.to_string()).or_insert_with(|| path.clone());
            }
        }
        let mut module_needs = BTreeSet::new();
        for dir in module_dirs(ld_cache) {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "so") {
                    module_needs.extend(verdef::read_needed(&path).unwrap_or_default());
                }
            }
        }
        HostIndex {
            libs,
            icd_libs,
            module_needs,
        }
    }

    /// The interpreter the host pair runs: its libc's `PT_INTERP` when
    /// it names a path that exists here, else the cache's own loader
    /// soname. An existing path comes back whatever it is, so closing it
    /// records why it cannot run instead of silently falling back to the
    /// bundled pair.
    fn loader_for(&self, host_libc: &Path) -> Option<PathBuf> {
        if let Some(interp) = crate::elf::read_interp(host_libc) {
            let interp = PathBuf::from(interp);
            if interp.exists() {
                return Some(interp);
            }
        }
        self.libs
            .iter()
            .find(|(name, path)| ldso::is_glibc_loader_name(name) && path.is_file())
            .map(|(_, path)| path.clone())
    }

    /// Every soname reachable through `DT_NEEDED` from the driver
    /// families, the host's dlopen-only seeds, the ICD-named drivers and
    /// what the host's driver modules need.
    fn driver_closure(&self) -> HashSet<String> {
        let mut queue: VecDeque<String> = self
            .libs
            .keys()
            .filter(|name| is_driver_family(name) || HOST_SEEDS.iter().any(|p| name.starts_with(p)))
            .cloned()
            .collect();
        for path in &self.icd_libs {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                queue.push_back(name.to_string());
            }
        }
        queue.extend(self.module_needs.iter().cloned());
        self.closure(queue)
    }

    /// Every soname reachable through `DT_NEEDED` from `seeds`, the seeds
    /// included.
    fn closure(&self, seeds: impl IntoIterator<Item = String>) -> HashSet<String> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<String> = seeds.into_iter().collect();
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

/// The largest name service configuration read. A real one is a few
/// hundred bytes.
const NSSWITCH_MAX_BYTES: u64 = 64 * 1024;

/// The most lines of the name service configuration looked at. A real one
/// is a few dozen.
const NSSWITCH_MAX_LINES: usize = 1024;

/// The longest service name taken from the name service configuration.
const NSS_SERVICE_MAX_LEN: usize = 64;

/// What a host libc opens by name rather than through `DT_NEEDED`: the
/// module of every service the name service configuration at `nsswitch`
/// names, and `libgcc_s`, which it opens to unwind a thread on cancellation.
/// A configuration that cannot be read, or is larger than any real one,
/// names no service.
fn host_libc_opens(nsswitch: &Path) -> Vec<String> {
    let mut opens: BTreeSet<String> = BTreeSet::new();
    opens.insert("libgcc_s.so.1".to_string());
    let text = read_nsswitch(nsswitch).unwrap_or_default();
    for line in text.lines().take(NSSWITCH_MAX_LINES) {
        let line = line.split('#').next().unwrap_or_default();
        let Some((_, sources)) = line.split_once(':') else {
            continue;
        };
        let mut in_action = false;
        for word in sources.split_whitespace() {
            if word.starts_with('[') {
                in_action = true;
            }
            let service = !in_action
                && word.len() <= NSS_SERVICE_MAX_LEN
                && word
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
            if service && !word.is_empty() {
                opens.insert(format!("libnss_{word}.so.2"));
            }
            if word.ends_with(']') {
                in_action = false;
            }
        }
    }
    opens.into_iter().collect()
}

/// The name service configuration's text, when it is a regular file no
/// larger than [`NSSWITCH_MAX_BYTES`] holding UTF-8.
fn read_nsswitch(path: &Path) -> Option<String> {
    use std::io::Read;
    let file = fs::File::open(path).ok()?;
    let md = file.metadata().ok()?;
    if !md.is_file() || md.len() > NSSWITCH_MAX_BYTES {
        return None;
    }
    let mut text = String::new();
    file.take(NSSWITCH_MAX_BYTES)
        .read_to_string(&mut text)
        .ok()?;
    Some(text)
}

/// The `dri` directories beside the host's library directories, where
/// Mesa keeps its DRI drivers and libva its VA-API drivers. Each sits next
/// to the library directory Mesa was installed into, so the loader cache
/// finds them on any distribution layout.
fn module_dirs(ld_cache: &Path) -> Vec<PathBuf> {
    drivers::cache_dirs_in(ld_cache)
        .into_iter()
        .map(|dir| Path::new(&dir).join("dri"))
        .filter(|dir| dir.is_dir())
        .collect()
}

/// The host counterpart directories of the package's carried plugin
/// directories: each compiled-in absolute path, and the directory's
/// path under every library directory the loader cache names, which
/// covers a multiarch host where `/usr/lib/alsa-lib` has
/// `/usr/lib/x86_64-linux-gnu/alsa-lib` beside it. Only those that
/// exist are returned, like the driver directories: a directory that
/// is not there holds no modules for the decision to weigh, and its
/// absence is the host's business alone.
fn counterpart_dirs(plugin_dirs: &[PluginDir], ld_cache: &Path) -> Vec<PathBuf> {
    if plugin_dirs.is_empty() {
        return Vec::new();
    }
    let libs = drivers::cache_dirs_in(ld_cache);
    let mut out: BTreeSet<PathBuf> = BTreeSet::new();
    for dir in plugin_dirs {
        for absolute in &dir.absolutes {
            out.insert(PathBuf::from(absolute));
        }
        if !dir.lib_relative.is_empty() {
            for lib in &libs {
                out.insert(Path::new(lib).join(&dir.lib_relative));
            }
        }
    }
    out.into_iter().filter(|dir| dir.is_dir()).collect()
}

/// The module files a directory holds: regular files whose name ends in
/// `.so`, sorted. A directory that cannot be read holds none.
fn modules_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|e| e == "so")
                && fs::metadata(path).is_ok_and(|md| md.is_file())
        })
        .collect();
    out.sort();
    out
}

/// The facts of one object, read once per decision. A file whose facts
/// cannot be read is remembered as unreadable.
fn facts_of(path: &Path, cache: &mut BTreeMap<PathBuf, Option<ElfFacts>>) -> Option<ElfFacts> {
    cache
        .entry(path.to_path_buf())
        .or_insert_with(|| verdef::read_facts(path).ok())
        .clone()
}

/// The versions the copy of `dependency` this launch would load
/// defines, or `None` when nothing supplies it.
///
/// The copy that loads is the one the decision chose: its winner, when
/// there is one, which for a glibc loader soname is the loader the
/// pair runs (the interpreter when the host's pair won, the bundle's
/// own loader copy under the bundled pair) rather than whatever entry
/// the cache carries; the host's family copy under its pair whenever
/// the host has one; the bundled copy otherwise. `loader` is the
/// interpreter the host's pair runs, when that pair won, and `None` is
/// the bundled pair. A copy whose facts cannot be read supplies
/// nothing: the launch would keep it or link it, and nothing could
/// vouch for its versions. Without a winner the bundle keeps a soname
/// it carries, and the bundled copy's versions are the conservative
/// measure, because a host copy only wins by defining a superset.
fn supplier_versions(
    dependency: &str,
    bundled: &BTreeMap<String, PathBuf>,
    host: &HostIndex,
    winners: &BTreeMap<String, PathBuf>,
    loader: Option<&Path>,
    facts: &mut BTreeMap<PathBuf, Option<ElfFacts>>,
) -> Option<VersionSet> {
    if ldso::is_glibc_loader_name(dependency) {
        let path = if loader.is_some() {
            loader
        } else {
            bundled.get(dependency).map(|p| p.as_path())
        };
        return path
            .and_then(|path| facts_of(path, facts))
            .filter(|f| f.mappable)
            .map(|f| f.defined_versions);
    }
    if let Some(path) = winners.get(dependency) {
        return facts_of(path, facts)
            .filter(|f| f.mappable)
            .map(|f| f.defined_versions);
    }
    if is_libc_family(dependency) {
        if loader.is_some()
            && let Some(path) = host.libs.get(dependency)
        {
            return facts_of(path, facts)
                .filter(|f| f.mappable)
                .map(|f| f.defined_versions);
        }
        return bundled
            .get(dependency)
            .and_then(|path| facts_of(path, facts))
            .filter(|f| f.mappable)
            .map(|f| f.defined_versions);
    }
    if !bundled.contains_key(dependency) {
        return host
            .libs
            .get(dependency)
            .and_then(|path| facts_of(path, facts))
            .filter(|f| f.mappable)
            .map(|f| f.defined_versions);
    }
    bundled
        .get(dependency)
        .and_then(|path| facts_of(path, facts))
        .filter(|f| f.mappable)
        .map(|f| f.defined_versions)
}

/// Admit or refuse each module the counterpart directories hold.
///
/// A module is admitted when every object of its closure through the
/// host's libraries has the bundled pair's class and machine, is of no
/// foreign libc family, carries no old-style search path the loader
/// searches ahead of the farm, names no filter object outside the
/// closure, demands only versions the copy that would load defines,
/// and needs only sonames something supplies: a decision
/// winner, a bundled copy, a glibc loader soname, which loads the
/// loader the pair runs when a host pair won, or a host copy, which is
/// then chosen like any other winner. Under the bundled pair (`loader`
/// `None`) the exception is a family member the bundle lacks, which
/// nothing can supply: the family is not farmed piecemeal. A refusal
/// names the first object that failed and why. The host sonames the
/// admitted closures reach come back, to join the decision's
/// candidates.
///
/// Without a bundled pair to check against there is no admission:
/// every module is refused. `loader` is the interpreter the host's
/// pair runs, when that pair won.
fn admit_modules(
    host: &HostIndex,
    bundled: &BTreeMap<String, PathBuf>,
    winners: &BTreeMap<String, PathBuf>,
    dirs: &[PathBuf],
    reference: Option<&Path>,
    loader: Option<&Path>,
) -> (Vec<PathBuf>, Vec<ModuleRefusal>, BTreeSet<String>) {
    let mut admitted: Vec<PathBuf> = Vec::new();
    let mut refused: Vec<ModuleRefusal> = Vec::new();
    let mut candidates: BTreeSet<String> = BTreeSet::new();
    let Some(pair) = reference.and_then(|path| verdef::read_facts(path).ok()) else {
        for dir in dirs {
            for module in modules_in(dir) {
                refused.push(ModuleRefusal {
                    object: module.clone(),
                    module,
                    reason: "no bundled pair to check against".to_string(),
                });
            }
        }
        refused.sort();
        return (admitted, refused, candidates);
    };
    let mut facts: BTreeMap<PathBuf, Option<ElfFacts>> = BTreeMap::new();
    for dir in dirs {
        for module in modules_in(dir) {
            match closure_verdict(&module, host, bundled, winners, &pair, loader, &mut facts) {
                Ok(reached) => {
                    candidates.extend(reached);
                    admitted.push(module);
                }
                Err(refusal) => refused.push(refusal),
            }
        }
    }
    admitted.sort();
    refused.sort();
    (admitted, refused, candidates)
}

/// Whether `module`'s closure through the host's libraries passes the
/// checks: `Ok` with the host sonames the closure reaches, to join the
/// decision's candidates, or `Err` with the first failing object and
/// its reason. A soname the decision already chose a winner for loads
/// from that winner, the farm linking it over the bundle's own copy,
/// so its closure is walked here. A soname without a winner that the
/// bundle carries loads from the bundle, whose closure is the bundle's
/// business, validated at launch. `loader` is the interpreter the
/// host's pair runs, when that pair won: only then can a family member
/// the bundle lacks be supplied at all, and only then does a glibc
/// loader soname have a copy to name. `None` is the bundled pair.
fn closure_verdict(
    module: &Path,
    host: &HostIndex,
    bundled: &BTreeMap<String, PathBuf>,
    winners: &BTreeMap<String, PathBuf>,
    pair: &ElfFacts,
    loader: Option<&Path>,
    facts: &mut BTreeMap<PathBuf, Option<ElfFacts>>,
) -> Result<BTreeSet<String>, ModuleRefusal> {
    let refuse = |object: PathBuf, reason: String| ModuleRefusal {
        module: module.to_path_buf(),
        object,
        reason,
    };
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut reached: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    queue.push_back(module.to_path_buf());
    while let Some(path) = queue.pop_front() {
        if !seen.insert(path.clone()) {
            continue;
        }
        let Some(object) = facts_of(&path, facts) else {
            return Err(refuse(path, "cannot be read".to_string()));
        };
        if object.class != pair.class || object.machine != pair.machine {
            return Err(refuse(
                path,
                "is not built for the package's architecture".to_string(),
            ));
        }
        match object.libc_family {
            LibcFamily::Glibc | LibcFamily::Unknown => {}
            LibcFamily::Musl => return Err(refuse(path, "is not of the glibc family".to_string())),
            LibcFamily::Conflicting => return Err(refuse(path, "mixes libc families".to_string())),
        }
        // An object without a load segment cannot be opened as a shared
        // object at all: admitting it would present a module that fails
        // at launch instead of a refusal here.
        if !object.mappable {
            return Err(refuse(
                path,
                "has no load segment, so the loader cannot map it".to_string(),
            ));
        }
        // The loader searches an object's old-style `DT_RPATH` ahead of
        // the farm, unless the object also carries a runpath, which
        // supersedes it. A filter object loads its objects outside this
        // closure: either way would bring in copies the checks never
        // saw.
        if object.has_rpath && !object.has_runpath {
            return Err(refuse(
                path,
                "has an old-style search path, which is searched ahead of the farm".to_string(),
            ));
        }
        if object.filtered {
            return Err(refuse(
                path,
                "has a filter object, which loads dependencies the closure does not name"
                    .to_string(),
            ));
        }
        for (dependency, required) in &object.required_versions {
            let Some(defined) =
                supplier_versions(dependency, bundled, host, winners, loader, facts)
            else {
                return Err(refuse(
                    path,
                    format!("needs {dependency}, which nothing supplies"),
                ));
            };
            let missing = required
                .iter()
                .filter(|r| !r.is_weak())
                .find(|r| !defined.contains(&r.name));
            if let Some(missing) = missing {
                return Err(refuse(
                    path,
                    format!(
                        "needs {dependency} version {}, which nothing here defines",
                        missing.name
                    ),
                ));
            }
        }
        for name in &object.needed {
            // The copy that loads is the decision's winner when there
            // is one: the farm links it over the bundle's own copy, so
            // its closure is walked here, whatever the bundle carries.
            // Without a winner a soname the bundle carries loads from
            // the bundle, and its closure is the bundle's business,
            // validated at launch.
            if let Some(winner) = winners.get(name) {
                queue.push_back(winner.clone());
                continue;
            }
            if bundled.contains_key(name) {
                continue;
            }
            // A glibc loader soname loads the loader the pair runs,
            // whatever the cache's entry for the name carries: the
            // closure walks the interpreter, and the soname is reached
            // so the farm links the closed copy under it. With only
            // the bundled pair the bundle's loader copy supplied it
            // above, or nothing does.
            if ldso::is_glibc_loader_name(name) {
                if let Some(loader) = loader {
                    reached.insert(name.clone());
                    queue.push_back(loader.to_path_buf());
                    continue;
                }
                // With only the bundled pair the demand is answered by
                // the loader the process itself runs under, and only by
                // it: the mapped loader carries its own soname, so a
                // foreign glibc loader name would still have to load
                // from disk. Version demands of it stay refused: the
                // mapped loader's definitions are not on disk to read.
                let own = pair
                    .interpreter
                    .as_deref()
                    .and_then(|interp| Path::new(interp).file_name())
                    .and_then(|name| name.to_str());
                if own == Some(name.as_str()) {
                    continue;
                }
                return Err(refuse(
                    path,
                    format!("needs {name}, which nothing supplies"),
                ));
            }
            // A family member rides with the pair that runs. Under the
            // bundled pair nothing outside the farm can be found, so a
            // family soname the bundle lacks cannot be supplied at all.
            if is_libc_family(name) && loader.is_none() {
                return Err(refuse(
                    path,
                    format!("needs {name}, which nothing supplies"),
                ));
            }
            match host.libs.get(name) {
                Some(host_path) => {
                    reached.insert(name.clone());
                    queue.push_back(host_path.clone());
                }
                None => {
                    return Err(refuse(
                        path,
                        format!("needs {name}, which nothing supplies"),
                    ));
                }
            }
        }
    }
    Ok(reached)
}

/// `library_path` values from Vulkan ICD and EGL vendor files under
/// `icd_dirs`. A value with a `/` names a file, absolute or relative to
/// the file; a bare soname is looked up in `bare_names` and dropped when
/// that is `None` or lacks it.
fn icd_library_paths_in(
    icd_dirs: &[&str],
    bare_names: Option<&BTreeMap<String, PathBuf>>,
) -> Vec<PathBuf> {
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
                if let Some(lib) = bare_names.and_then(|names| names.get(value)) {
                    out.push(lib.clone());
                }
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
pub fn json_library_path(text: &str) -> Option<&str> {
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
        for (name, path) in libs_in(&pkg_root.join(dir)) {
            out.entry(name).or_insert(path);
        }
    }
    out
}

/// Every shared object directly in `dir`, by file name.
///
/// A symlink to an absolute path is not one of them. A package's own links
/// stay inside it, and an absolute one is a link the runtime made to a host
/// or set library, which a mount or a cache tree still shows when a later
/// launch looks.
fn libs_in(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str().map(String::from)?;
            let path = entry.path();
            let placed = fs::read_link(&path).is_ok_and(|t| t.is_absolute());
            (name.contains(".so") && !placed && path.is_file()).then_some((name, path))
        })
        .collect()
}

/// A description of everything on the host the decision depends on: the
/// cache image, the name service configuration, the directories the
/// cache names, the driver description files and the counterparts of the
/// package's carried plugin directories with their module files, each
/// with its size and modification time, and where loader copies can be
/// kept. Compared whole, so no hash is needed. The stamps carry the
/// change time as well, which a rewrite always moves and a tool cannot
/// pin back, so a rewrite beneath a restored modification time still
/// recomputes. A refusal's evidence in the host's own libraries is not
/// stamped: it re-examines when any watched host state changes, and
/// until then the record keeps refusing, which is the safe direction.
/// Every argument is distinct and a parameter object would be built at
/// each call site and read once.
#[allow(clippy::too_many_arguments)]
fn fingerprint(
    ld_cache: &Path,
    plugin_dirs: &[PluginDir],
    icd_dirs: &[&str],
    policy: HostLibsPolicy,
    extra_root: Option<&Path>,
    host_libc: bool,
    nsswitch: &Path,
    loader_store: LoaderStore,
) -> String {
    let mut out = format!("onelf-resolve-1 {}", policy.as_str());
    if !host_libc {
        out.push_str(" bundled-libc");
    }
    match loader_store {
        LoaderStore::Root { root, .. } => {
            out.push_str(&format!(" loader-store={}", root.display()));
        }
        LoaderStore::Refused(_) => out.push_str(" loader-store=refused"),
    }
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
                use std::os::unix::fs::MetadataExt;
                out.push_str(&format!(
                    "={}:{mtime}:{}.{}",
                    md.len(),
                    md.ctime(),
                    md.ctime_nsec()
                ));
            }
            Err(_) => out.push_str("=missing"),
        }
    };
    stamp(ld_cache);
    stamp(nsswitch);
    for dir in drivers::cache_dirs_in(ld_cache) {
        stamp(Path::new(&dir));
    }
    for dir in module_dirs(ld_cache) {
        stamp(&dir);
    }
    for dir in counterpart_dirs(plugin_dirs, ld_cache) {
        stamp(&dir);
        // The module files themselves, so an in-place rewrite at the
        // same size, invisible to the directory stamp, still recomputes
        // the decision.
        for module in modules_in(&dir) {
            stamp(&module);
        }
    }
    for dir in icd_dirs {
        stamp(Path::new(dir));
    }
    if let Some(root) = extra_root {
        for dir in GL_BUILD_LIB_DIRS.iter().chain(GL_BUILD_ICD_DIRS) {
            stamp(&root.join(dir));
        }
    }
    out
}

const DECISION_FILE: &str = "decision";

/// The record format [`resolve_selected`] reads and writes, keyed on the
/// host and the launch's selection identity. The version in the header
/// steps up whenever the layout changes, so a record naming another
/// version is a miss, never a misread.
const SELECTION_RECORD_HEADER: &str = "onelf-resolve-selection 3";

/// The store subdirectory a selection's record lives in, named by the
/// identity's key. Beside the farms, outside the `onelf-` mountpoint
/// namespace the sweep reclaims.
const SELECTION_DIR_PREFIX: &str = "sel-";

/// How many active pins an identity may carry, matching the per-selection
/// count the package metadata enforces on declared and selected sets.
const MAX_ACTIVE_PINS: usize = 4096;

/// How many bytes an active pin's name may carry, matching the name
/// bound the package metadata enforces.
const MAX_PIN_LABEL_BYTES: usize = 128;

/// How many digest bytes a selection's directory key keeps.
const SELECTION_KEY_BYTES: usize = 16;

/// The largest record a read accepts, so hostile state is a miss rather
/// than a large read.
const RECORD_MAX_BYTES: u64 = 1024 * 1024;

/// The link farm for one host state. Keyed by the fingerprint and the
/// winners themselves: two decisions can share a fingerprint (an
/// in-place library swap leaves the directory stamps alone) yet pick
/// different winners, and a farm reused across them would serve the
/// previous decision's stale links. A launch after the host changed
/// builds a new one beside the old rather than replacing it: an
/// instance started earlier may still be loading through the old farm,
/// and two instances starting together build the same one and race for
/// nothing. The closed host loader copy its loader link names keys it
/// too, so a farm only ever names the copy it was built for.
fn farm_dir(
    store: &Path,
    fingerprint: &str,
    winners: &BTreeMap<String, PathBuf>,
    loader_copy: Option<&Path>,
) -> PathBuf {
    // FNV-1a, so the names are stable across runs without a hash
    // dependency.
    let mix = |h: &mut u64, b: u8| {
        *h ^= u64::from(b);
        *h = h.wrapping_mul(0x0100_0000_01b3);
    };
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in fingerprint.bytes() {
        mix(&mut h, b);
    }
    let mut w: u64 = 0xcbf2_9ce4_8422_2325;
    for (name, path) in winners {
        for b in name.bytes().chain(path.to_string_lossy().bytes()) {
            mix(&mut w, b);
        }
        mix(&mut w, 0x1f);
    }
    for b in loader_copy
        .map(|p| p.as_os_str().as_bytes())
        .unwrap_or_default()
    {
        mix(&mut w, *b);
    }
    store.join(format!("farm-{h:016x}-{w:016x}"))
}

/// The cache root subdirectory closed host loader copies are kept in,
/// one directory per closed image's digest.
const LOADERS_DIR: &str = "loaders";

/// Where under the cache `root` the closed image with digest `hash` of a
/// loader named `name` is kept.
fn loader_copy(root: &Path, hash: &blake3::Hash, name: &str) -> PathBuf {
    root.join(LOADERS_DIR)
        .join(hash.to_hex().as_str())
        .join(name)
}

/// The file name a host loader's copy keeps, so the process's mappings
/// still name the loader they came from.
fn loader_name(source: &Path) -> String {
    source
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| crate::manifest::is_safe_component(n))
        .unwrap_or("ld.so")
        .to_string()
}

/// Where the farm link for winner `name` points: the closed copy for the
/// loader when one runs, the chosen host file for everything else. The
/// copy is what a bundled loader's path shows once it is shadowed, so a
/// process that re-execs through that path gets the closed loader too.
fn link_target<'p>(name: &str, host: &'p Path, copy: Option<&'p Path>) -> &'p Path {
    match copy {
        Some(copy) if ldso::is_glibc_loader_name(name) => copy,
        _ => host,
    }
}

/// A loader image read whole, refused when it is not a regular file or
/// is larger than any loader.
fn read_loader(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    // Opened nonblocking first, so a named pipe or device a hostile
    // interpreter names cannot hang the open, then refused unless it is
    // a regular file.
    let file = fs::File::options()
        .read(true)
        .custom_flags(0o4000)
        .open(path)?;
    let md = file.metadata()?;
    if !md.is_file() || md.len() > MAX_LOADER_BYTES as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a regular file of a loader's size",
        ));
    }
    let mut image = Vec::with_capacity(md.len() as usize);
    file.take(MAX_LOADER_BYTES as u64 + 1)
        .read_to_end(&mut image)?;
    if image.len() > MAX_LOADER_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "grew past a loader's size while read",
        ));
    }
    Ok(image)
}

/// The image of the host loader at `source` closed to its default search
/// directories, or why it cannot be. An image in which no default
/// directory is recognized is refused: nothing then shows that its search
/// is closed.
fn close_loader(source: &Path) -> Result<Vec<u8>, String> {
    let mut image = read_loader(source).map_err(|e| format!("it cannot be read: {e}"))?;
    let scrub = ldso::scrub_glibc_loader(&mut image).map_err(|e| e.to_string())?;
    if scrub.default_dirs == 0 {
        return Err("its image names no default search directory the runtime can close".into());
    }
    Ok(image)
}

/// Write `image` to `path` with execute permission, through a temporary
/// name beside it, so a reader never sees a partial copy.
fn write_loader(path: &Path, image: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = path.parent().unwrap_or(Path::new("."));
    let n = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".loader.{}.{n}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&tmp)?;
    let written = file.write_all(image).and_then(|()| {
        drop(file);
        fs::rename(&tmp, path)
    });
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

/// The link farm for `fingerprint` holding `winners`, built if it does not
/// exist yet, with its loader links naming `copy` when the host's pair
/// won. An empty set leaves no farm.
///
/// Built in a scratch directory and renamed into place. A rename that finds
/// the farm already there lost to another instance building the same
/// thing, which is fine. Nothing is ever removed.
fn materialize(
    store: &Path,
    fingerprint: &str,
    winners: &BTreeMap<String, PathBuf>,
    copy: Option<&Path>,
) -> io::Result<Option<PathBuf>> {
    if winners.is_empty() {
        return Ok(None);
    }
    let farm = farm_dir(store, fingerprint, winners, copy);
    // A farm left by an earlier launch is reused only when every link
    // still names exactly the chosen copy, the way a served record's
    // farm is checked: the launch searches whatever the directory holds.
    if farm_checked(store, fingerprint, winners, copy).is_some() {
        return Ok(Some(farm));
    }
    static SCRATCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SCRATCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let scratch = store.join(format!(
        ".{}.{}.{}",
        farm.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        n
    ));
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch)?;
    for (name, target) in winners {
        std::os::unix::fs::symlink(link_target(name, target, copy), scratch.join(name))?;
    }
    match fs::rename(&scratch, &farm) {
        Ok(()) => {}
        Err(_) => {
            // The rename lost to a farm that appeared meanwhile. When
            // that farm checks out it wins; otherwise it is the damaged
            // one this build exists to replace, so rebuild over it.
            if farm_checked(store, fingerprint, winners, copy).is_some() {
                let _ = fs::remove_dir_all(&scratch);
            } else {
                // The farm path may be held by a single stray file,
                // which remove_dir_all cannot clear.
                match fs::symlink_metadata(&farm) {
                    Ok(md) if !md.is_dir() => {
                        let _ = fs::remove_file(&farm);
                    }
                    _ => {
                        let _ = fs::remove_dir_all(&farm);
                    }
                }
                if let Err(e) = fs::rename(&scratch, &farm) {
                    let _ = fs::remove_dir_all(&scratch);
                    return Err(e);
                }
            }
        }
    }
    Ok(Some(farm))
}

/// The merged plugin views of one host state, keyed by the fingerprint,
/// the admitted modules themselves and the package tree the bundled
/// members aim at, the way [`farm_dir`] keys a farm: a changed decision
/// builds beside the old rather than replacing it, an older instance may
/// still be reading through the one it was given, and a decision whose
/// admitted set or package tree changed never shares one. The package
/// tree is part of the key because a view's bundled members aim into
/// it, and a launch tree belongs to its launch alone.
fn views_dir(store: &Path, fingerprint: &str, admitted: &[PathBuf], pkg_root: &Path) -> PathBuf {
    // FNV-1a, so the names are stable across runs without a hash
    // dependency.
    let mix = |h: &mut u64, b: u8| {
        *h ^= u64::from(b);
        *h = h.wrapping_mul(0x0100_0000_01b3);
    };
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in fingerprint.bytes() {
        mix(&mut h, b);
    }
    let mut w: u64 = 0xcbf2_9ce4_8422_2325;
    for path in admitted {
        for b in path.to_string_lossy().bytes() {
            mix(&mut w, b);
        }
        mix(&mut w, 0x1f);
    }
    for b in pkg_root.to_string_lossy().bytes() {
        mix(&mut w, b);
    }
    store.join(format!("view-{h:016x}-{w:016x}"))
}

/// Whether `dir` links exactly `expected`, names and targets, and
/// nothing else.
fn view_checked(dir: &Path, expected: &[(OsString, PathBuf)]) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    let got: BTreeMap<OsString, PathBuf> = entries
        .flatten()
        .map(|entry| (entry.file_name(), entry.path()))
        .collect();
    got.len() == expected.len()
        && expected.iter().all(|(name, target)| {
            got.get(name)
                .is_some_and(|link| fs::read_link(link).is_ok_and(|aimed| &aimed == target))
        })
}

/// One record's pending view: its directory name under the decision's
/// view root, the record's package path, and the entries the view
/// links.
struct WantedView {
    name: String,
    relative: String,
    entries: Vec<(OsString, PathBuf)>,
}

/// The view root for the wanted views under `root`: verified and reused
/// when every view's directory links exactly its entries, built beside
/// and renamed over the standing tree otherwise. Each view's directory
/// name is unique inside the root.
fn views_materialized(root: &Path, wanted: &[WantedView]) -> io::Result<()> {
    let stands = |root: &Path| {
        root.is_dir()
            && wanted
                .iter()
                .all(|w| view_checked(&root.join(&w.name), &w.entries))
    };
    if stands(root) {
        return Ok(());
    }
    static SCRATCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SCRATCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let scratch = root.parent().unwrap_or(root).join(format!(
        ".{}.{}.{}",
        root.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        n
    ));
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch)?;
    for w in wanted {
        let dir = scratch.join(&w.name);
        fs::create_dir(&dir)?;
        for (member, target) in &w.entries {
            std::os::unix::fs::symlink(target, dir.join(member))?;
        }
    }
    match fs::rename(&scratch, root) {
        Ok(()) => {}
        Err(first) => {
            // The rename lost to a root that appeared meanwhile. When
            // that root checks out it wins; otherwise it is the damaged
            // one this build exists to replace, so rebuild over it.
            // Siblings can hold the root mid-rebuild for a few rounds,
            // so the look, the clear and the rename retry a bounded
            // number of times before this build gives up.
            let mut last = first;
            for _ in 0..3 {
                if stands(root) {
                    let _ = fs::remove_dir_all(&scratch);
                    return Ok(());
                }
                // The root path may be held by a single stray file,
                // which remove_dir_all cannot clear.
                match fs::symlink_metadata(root) {
                    Ok(md) if !md.is_dir() => {
                        let _ = fs::remove_file(root);
                    }
                    _ => {
                        let _ = fs::remove_dir_all(root);
                    }
                }
                match fs::rename(&scratch, root) {
                    Ok(()) => return Ok(()),
                    Err(e) => last = e,
                }
            }
            // A sibling launch's rebuild may have landed in between:
            // when the standing root checks out, it wins and this
            // build's scratch is dropped.
            let _ = fs::remove_dir_all(&scratch);
            if !stands(root) {
                return Err(last);
            }
        }
    }
    Ok(())
}

/// The counterpart directories of one record that exist on this host:
/// its compiled-in absolute paths, and its library-relative path under
/// every loader-cache library directory.
fn record_counterpart_dirs(dir: &PluginDir, libs: &[String]) -> Vec<PathBuf> {
    let mut out: BTreeSet<PathBuf> = BTreeSet::new();
    for absolute in &dir.absolutes {
        out.insert(PathBuf::from(absolute));
    }
    if !dir.lib_relative.is_empty() {
        for lib in libs {
            out.insert(Path::new(lib).join(&dir.lib_relative));
        }
    }
    out.into_iter().filter(|dir| dir.is_dir()).collect()
}

/// The entries a view of `relative` holds, sorted by member name: every
/// bundled member aimed at the package's copy, then every admitted host
/// member whose name no bundled member holds, aimed at the host's file.
/// `None` when the package does not carry the directory, in which case
/// no redirection would reach the view either.
fn view_entries(
    pkg_root: &Path,
    relative: &str,
    members: &[&PathBuf],
) -> Option<Vec<(OsString, PathBuf)>> {
    let bundled = pkg_root.join(relative);
    let read = fs::read_dir(&bundled).ok()?;
    let mut out: Vec<(OsString, PathBuf)> = Vec::new();
    let mut names: BTreeSet<OsString> = BTreeSet::new();
    for entry in read.flatten() {
        let name = entry.file_name();
        names.insert(name.clone());
        out.push((name, bundled.join(entry.file_name())));
    }
    for member in members {
        let Some(name) = member.file_name() else {
            continue;
        };
        if names.insert(name.to_os_string()) {
            out.push((name.to_os_string(), (*member).clone()));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Some(out)
}

/// The merged plugin views of one decision: one per plugin directory
/// whose counterpart directories hold admitted modules, under one
/// decision-keyed root in the store. A record whose bundled directory
/// cannot be read is left out: nothing would reach its view. A root
/// that cannot be written leaves every directory's bundled members
/// alone, which loses the host modules of the launch, never the
/// package's.
fn plugin_views(req: &Request, fingerprint: &str, admitted: &[PathBuf]) -> Vec<PluginView> {
    if admitted.is_empty() || req.plugin_dirs.is_empty() {
        return Vec::new();
    }
    let libs = drivers::cache_dirs_in(req.ld_cache);
    let mut wanted: Vec<WantedView> = Vec::new();
    for (index, dir) in req.plugin_dirs.iter().enumerate() {
        let dirs = record_counterpart_dirs(dir, &libs);
        let members: Vec<&PathBuf> = admitted
            .iter()
            .filter(|module| {
                module
                    .parent()
                    .is_some_and(|parent| dirs.contains(&parent.to_path_buf()))
            })
            .collect();
        if members.is_empty() {
            continue;
        }
        let Some(entries) = view_entries(req.pkg_root, &dir.relative, &members) else {
            continue;
        };
        wanted.push(WantedView {
            name: format!("dir-{index}"),
            relative: dir.relative.clone(),
            entries,
        });
    }
    if wanted.is_empty() {
        return Vec::new();
    }
    let root = views_dir(req.store, fingerprint, admitted, req.pkg_root);
    if let Err(e) = views_materialized(&root, &wanted) {
        eprintln!(
            "onelf-rt: resolver: cannot build the merged plugin views in {}: {e}",
            root.display()
        );
        return Vec::new();
    }
    wanted
        .into_iter()
        .map(|w| PluginView {
            relative: w.relative,
            dir: root.join(w.name),
        })
        .collect()
}

/// Attach a decision's merged plugin views to its resolution.
fn with_plugin_views(mut resolution: Resolution, req: &Request, fingerprint: &str) -> Resolution {
    resolution.plugin_views = plugin_views(req, fingerprint, &resolution.admitted);
    resolution
}

/// Make sure `loader`'s copy holds its closed image, writing it when it is
/// absent or does not match. Every directory from the cache root down to the
/// copy's has to be a `0700` one the store's uid owns, as the cache's own
/// are.
fn keep_loader_copy(loader: &ClosedLoader, store: LoaderStore) -> io::Result<()> {
    let LoaderStore::Root { root, uid } = store else {
        return Err(io::Error::other("no loader store"));
    };
    let dir = loader.copy.parent().unwrap_or(root);
    let loaders = dir.parent().unwrap_or(root);
    for d in [root, loaders, dir] {
        if !crate::cache_layout::ensure_safe_dir(d, uid) {
            return Err(io::Error::other(format!(
                "{} is not a private directory",
                d.display()
            )));
        }
    }
    let copy = HostLoader {
        path: loader.copy.clone(),
        hash: loader.hash,
    };
    if copy_is_owned_file(&copy.path, uid) && copy.verify().is_ok() {
        return Ok(());
    }
    write_loader(&loader.copy, &loader.image)
        .map_err(|e| io::Error::new(e.kind(), format!("cannot write {}: {e}", dir.display())))
}

/// Whether `copy`, kept under the cache `root`, can be trusted by its
/// digest. Its contents are a function of the world-readable host loader,
/// so anyone can produce a file that hashes right; what makes it this
/// uid's is where it sits. Every directory from the root down to the
/// copy's has to be a `0700` one `uid` owns, so nobody else can swap the
/// file once it is checked, and the copy has to be a regular file `uid`
/// owns, not a link to one.
fn copy_is_private(root: &Path, copy: &HostLoader, uid: u32) -> bool {
    let loaders = root.join(LOADERS_DIR);
    let dir = loaders.join(copy.hash.to_hex().as_str());
    [root, &loaders, &dir]
        .iter()
        .all(|d| crate::cache_layout::is_safe_owned_dir(d, uid))
        && copy.path.parent() == Some(dir.as_path())
        && copy_is_owned_file(&copy.path, uid)
}

/// Whether `path` is a regular file `uid` owns, not a link to one, that
/// nobody else may write and that no other name shares, so nothing outside
/// its private directory can reach it.
fn copy_is_owned_file(path: &Path, uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(path).is_ok_and(|md| {
        md.is_file() && md.uid() == uid && md.mode() & 0o022 == 0 && md.nlink() == 1
    })
}

/// Write `resolution` and the winners it was decided from to `dir`'s
/// record, replacing any earlier one atomically. The identity lines come
/// first, then the host state the decision was made about.
fn write_decision(
    dir: &Path,
    header: &str,
    identity_text: &str,
    fingerprint: &str,
    resolution: &Resolution,
    winners: &BTreeMap<String, PathBuf>,
) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let mut text = String::new();
    text.push_str(header);
    text.push('\n');
    if !identity_text.is_empty() {
        text.push_str(identity_text);
        text.push('\n');
    }
    text.push_str(&format!("fingerprint {fingerprint}\n"));
    if let Some(loader) = &resolution.host_interp {
        let name = loader
            .path
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default();
        text.push_str(&format!("interp {}\t{name}\n", loader.hash.to_hex()));
    }
    for (name, path) in winners {
        text.push_str(&format!("host {name}\t{}\n", path.display()));
    }
    // The identity of every winner, so the next launch can tell an
    // in-place same-size swap (invisible to the directory stamps) from
    // the host the decision was made about.
    for (name, path) in winners {
        text.push_str(&format!("stat {name}\t{}\n", stat_line(path)));
    }
    // The count of admissions, so a record truncated before its module
    // section is a miss instead of a launch that silently presents
    // nothing.
    text.push_str(&format!("admitted {}\n", resolution.admitted.len()));
    for module in &resolution.admitted {
        text.push_str(&format!("module {}\n", module.display()));
    }
    // The identity of every admitted module, for the same reason the
    // winners carry one: an in-place same-size swap leaves the
    // counterpart directory stamps alone.
    for module in &resolution.admitted {
        text.push_str(&format!(
            "module-stat {}\t{}\n",
            module.display(),
            stat_line(module)
        ));
    }
    for name in &resolution.incomparable {
        text.push_str(&format!("incomparable {name}\n"));
    }
    let tmp = dir.join(format!(".{DECISION_FILE}.{}.{}", std::process::id(), {
        static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }));
    let _ = fs::remove_file(&tmp);
    fs::File::create(&tmp)?.write_all(text.as_bytes())?;
    fs::rename(&tmp, dir.join(DECISION_FILE))
}

/// Record a selection's decision under its identity.
fn record_selection(
    dir: &Path,
    identity: &SelectionIdentity,
    fingerprint: &str,
    resolution: &Resolution,
    winners: &BTreeMap<String, PathBuf>,
) -> io::Result<()> {
    write_decision(
        dir,
        SELECTION_RECORD_HEADER,
        identity.canonical(),
        fingerprint,
        resolution,
        winners,
    )
}

/// The identity of one file: size, inode, and both times, separated by
/// colons. `missing` when the file cannot be stat'ed at all.
fn stat_line(path: &Path) -> String {
    match fs::metadata(path) {
        Ok(md) => {
            use std::os::unix::fs::MetadataExt;
            format!(
                "{}:{}:{}:{}:{}:{}",
                md.len(),
                md.ino(),
                md.mtime(),
                md.mtime_nsec(),
                md.ctime(),
                md.ctime_nsec()
            )
        }
        Err(_) => "missing".to_string(),
    }
}

/// A selection's recorded decision, when it was made for this exact
/// identity and host state, everything it names still exists, and a
/// closed loader copy it names still matches its digest.
/// `farm_base` is the store the decision's farm lives in, and
/// `loader_store` where loader copies are kept.
fn load_selection(
    dir: &Path,
    identity: &SelectionIdentity,
    fingerprint: &str,
    farm_base: &Path,
    loader_store: LoaderStore,
) -> Option<Resolution> {
    let text = read_record_bounded(&dir.join(DECISION_FILE))?;
    let lines = identity.identity_lines();
    let (mut resolution, winners, loader) =
        parse_decision(&text, SELECTION_RECORD_HEADER, &lines, fingerprint)?;
    if winners.is_empty() {
        return loader.is_none().then_some(resolution);
    }
    let copy = match loader {
        Some((hash, name)) => {
            let LoaderStore::Root { root, uid } = loader_store else {
                return None;
            };
            let copy = HostLoader {
                path: loader_copy(root, &hash, &name),
                hash,
            };
            if !copy_is_private(root, &copy, uid) {
                return None;
            }
            copy.verify().ok()?;
            Some(copy)
        }
        None => None,
    };
    let copy_path = copy.as_ref().map(|c| c.path.as_path());
    resolution.farm = Some(farm_checked(farm_base, fingerprint, &winners, copy_path)?);
    resolution.host_interp = copy;
    Some(resolution)
}

/// The record's text, read only when it is a small regular file holding
/// valid UTF-8. Hostile or damaged state is a miss with a clean reason,
/// never a large read or a partial adoption.
fn read_record_bounded(path: &Path) -> Option<String> {
    let md = fs::metadata(path).ok()?;
    if !md.is_file() || md.len() > RECORD_MAX_BYTES {
        return None;
    }
    fs::read_to_string(path).ok()
}

/// A record's closed loader: the copy's digest and file name.
type RecordedLoader = (blake3::Hash, String);

/// The decision a record carries, when its version, identity lines and
/// host state all check out, together with the winners it was decided
/// from and the closed loader it runs, if any. Each admitted module is
/// named once, the record declares how many it admits, and every
/// module and every winner carries exactly one `stat` line that still
/// matches it, so a truncated, damaged, padded or stale record is a
/// miss and nothing is adopted partially. Each winner is named once on
/// a `host` line: a repeat would move the winner after its `stat` line
/// verified a different path.
fn parse_decision(
    text: &str,
    header: &str,
    identity_lines: &[&str],
    fingerprint: &str,
) -> Option<(
    Resolution,
    BTreeMap<String, PathBuf>,
    Option<RecordedLoader>,
)> {
    let mut lines = text.lines();
    if lines.next()? != header {
        return None;
    }
    for expected in identity_lines {
        if lines.next()? != *expected {
            return None;
        }
    }
    if lines.next()?.strip_prefix("fingerprint ")? != fingerprint {
        return None;
    }
    let mut resolution = Resolution::default();
    let mut winners: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut loader = None;
    let mut admitted_count: Option<usize> = None;
    let mut winner_stats: BTreeSet<String> = BTreeSet::new();
    let mut modules: BTreeSet<PathBuf> = BTreeSet::new();
    let mut verified: BTreeSet<PathBuf> = BTreeSet::new();
    for line in lines {
        if let Some(rest) = line.strip_prefix("interp ") {
            let (hash, name) = rest.split_once('\t')?;
            if loader.is_some() || !crate::manifest::is_safe_component(name) {
                return None;
            }
            loader = Some((blake3::Hash::from_hex(hash).ok()?, name.to_string()));
        } else if let Some(rest) = line.strip_prefix("admitted ") {
            let count = rest.parse::<usize>().ok()?;
            if admitted_count.is_some() {
                return None;
            }
            admitted_count = Some(count);
        } else if let Some(rest) = line.strip_prefix("host ") {
            let (name, path) = rest.split_once('\t')?;
            if winners.contains_key(name) {
                return None;
            }
            winners.insert(name.to_string(), PathBuf::from(path));
        } else if let Some(rest) = line.strip_prefix("stat ") {
            let (name, recorded) = rest.split_once('\t')?;
            let path = winners.get(name)?;
            if !winner_stats.insert(name.to_string()) || stat_line(path) != recorded {
                return None;
            }
        } else if let Some(rest) = line.strip_prefix("module ") {
            if !modules.insert(PathBuf::from(rest)) {
                return None;
            }
        } else if let Some(rest) = line.strip_prefix("module-stat ") {
            let (path, recorded) = rest.split_once('\t')?;
            let path = Path::new(path);
            if recorded == "missing"
                || !modules.contains(path)
                || !verified.insert(path.to_path_buf())
                || stat_line(path) != recorded
            {
                return None;
            }
        } else {
            let name = line.strip_prefix("incomparable ")?;
            resolution.incomparable.push(name.to_string());
        }
    }
    if admitted_count != Some(modules.len())
        || winner_stats.len() != winners.len()
        || verified.len() != modules.len()
    {
        return None;
    }
    resolution.admitted = modules.into_iter().collect();
    Some((resolution, winners, loader))
}

/// The farm for `fingerprint` holding `winners` and naming the loader
/// `copy`, when every link names what the decision chose and the chosen
/// file still exists.
fn farm_checked(
    base: &Path,
    fingerprint: &str,
    winners: &BTreeMap<String, PathBuf>,
    copy: Option<&Path>,
) -> Option<PathBuf> {
    let farm = farm_dir(base, fingerprint, winners, copy);
    for (name, path) in winners {
        let expected = link_target(name, path, copy);
        match fs::read_link(farm.join(name)) {
            Ok(target) if target == expected && path.is_file() => {}
            _ => return None,
        }
    }
    // The launch searches the whole farm, so any entry beyond the
    // winners' own links would put an unchecked copy on the search path.
    let entries = match fs::read_dir(&farm) {
        Ok(entries) => entries,
        Err(_) => return None,
    };
    let mut linked = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !winners.contains_key(name.as_ref()) {
            return None;
        }
        linked += 1;
    }
    if linked != winners.len() {
        return None;
    }
    Some(farm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verdef::fixture::{self, shared_object, shared_object_with_interp, temp_dir};
    use std::process::Command;

    /// A host with a library directory and a loader cache naming its
    /// contents, and a package root with one `lib/` directory.
    struct World {
        host_lib: PathBuf,
        cache: PathBuf,
        pkg: PathBuf,
        store: PathBuf,
        /// Where the host's ICD files go; empty unless a test writes one.
        icd_dir: PathBuf,
        /// The farm the last resolution produced, where `farm_link` looks.
        last_farm: std::cell::RefCell<Option<PathBuf>>,
        /// The cache root closed loader copies go under.
        loader_root: PathBuf,
        /// The host's name service configuration; absent unless a test
        /// writes one.
        nsswitch: PathBuf,
        /// The plugin directories the package carries, empty unless a
        /// test carries some with [`World::carry`].
        plugin_dirs: std::cell::RefCell<Vec<PluginDir>>,
        uid: u32,
    }

    impl World {
        fn new() -> Self {
            let root = temp_dir("resolve");
            let host_lib = root.join("host");
            let pkg = root.join("pkg");
            fs::create_dir_all(&host_lib).unwrap();
            fs::create_dir_all(pkg.join("lib")).unwrap();
            let icd_dir = root.join("icd.d");
            fs::create_dir_all(&icd_dir).unwrap();
            World {
                cache: root.join("ld.so.cache"),
                store: root.join("store"),
                host_lib,
                pkg,
                icd_dir,
                last_farm: std::cell::RefCell::new(None),
                loader_root: root.join("cache"),
                nsswitch: root.join("nsswitch.conf"),
                plugin_dirs: std::cell::RefCell::new(Vec::new()),
                uid: {
                    use std::os::unix::fs::MetadataExt;
                    fs::metadata(&root).unwrap().uid()
                },
            }
        }

        fn loader_store(&self) -> LoaderStore<'_> {
            LoaderStore::Root {
                root: &self.loader_root,
                uid: self.uid,
            }
        }

        /// Have the package carry plugin directories, whose host
        /// counterparts join the decision.
        fn carry(&self, dirs: Vec<PluginDir>) {
            *self.plugin_dirs.borrow_mut() = dirs;
        }

        fn host(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.host_lib.join(name);
            fs::write(&path, bytes).unwrap();
            self.write_cache();
            path
        }

        /// Put a driver module in the `dri` directory beside the host's
        /// libraries, where Mesa opens it by path.
        fn host_module(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let dir = self.host_lib.join("dri");
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join(name);
            fs::write(&path, bytes).unwrap();
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
                .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
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
            self.resolve_with(policy, None)
        }

        fn resolve_with(&self, policy: HostLibsPolicy, extra_root: Option<&Path>) -> Resolution {
            self.resolve_full(policy, extra_root, true)
        }

        /// [`decide`] for this world, with launches able to shadow.
        fn decide(&self, policy: HostLibsPolicy) -> Decision {
            let icd_dir = self.icd_dir.to_string_lossy().into_owned();
            decide(&Request {
                pkg_root: &self.pkg,
                lib_dirs: &["lib"],
                plugin_dirs: &self.plugin_dirs.borrow(),
                policy,
                store: &self.store,
                ld_cache: &self.cache,
                icd_dirs: &[icd_dir.as_str()],
                extra_root: None,
                set_roots: &[],
                host_libc: true,
                loader_store: self.loader_store(),
                nsswitch: &self.nsswitch,
            })
        }

        fn resolve_full(
            &self,
            policy: HostLibsPolicy,
            extra_root: Option<&Path>,
            host_libc: bool,
        ) -> Resolution {
            let icd_dir = self.icd_dir.to_string_lossy().into_owned();
            let set_roots = self.set_roots();
            let r = resolve_selected(
                &Request {
                    pkg_root: &self.pkg,
                    lib_dirs: &["lib"],
                    plugin_dirs: &self.plugin_dirs.borrow(),
                    policy,
                    store: &self.store,
                    ld_cache: &self.cache,
                    icd_dirs: &[icd_dir.as_str()],
                    extra_root,
                    set_roots: &set_roots,
                    host_libc,
                    loader_store: self.loader_store(),
                    nsswitch: &self.nsswitch,
                },
                &[],
            );
            *self.last_farm.borrow_mut() = r.farm.clone();
            r
        }

        /// [`resolve_selected`] against the given set roots and active
        /// pins, with the farm the launch produced remembered for
        /// [`World::farm_link`].
        fn resolve_selection(
            &self,
            policy: HostLibsPolicy,
            set_roots: &[PathBuf],
            pins: &[ActivePin],
        ) -> Resolution {
            let icd_dir = self.icd_dir.to_string_lossy().into_owned();
            let r = resolve_selected(
                &Request {
                    pkg_root: &self.pkg,
                    lib_dirs: &["lib"],
                    plugin_dirs: &self.plugin_dirs.borrow(),
                    policy,
                    store: &self.store,
                    ld_cache: &self.cache,
                    icd_dirs: &[icd_dir.as_str()],
                    extra_root: None,
                    set_roots,
                    host_libc: true,
                    loader_store: self.loader_store(),
                    nsswitch: &self.nsswitch,
                },
                pins,
            );
            *self.last_farm.borrow_mut() = r.farm.clone();
            r
        }

        fn set_roots(&self) -> Vec<PathBuf> {
            Vec::new()
        }

        fn gl(&self) -> Gl {
            gl_situation(&self.pkg, &["lib"], &[], None, &self.cache, &[])
        }

        fn farm_link(&self, name: &str) -> Option<PathBuf> {
            let farm = self.last_farm.borrow().clone()?;
            fs::read_link(farm.join(name)).ok()
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
            out.extend_from_slice(&drivers::native_entry_flags().to_le_bytes());
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

    /// A pin named `label` whose build is pinned by the digest `n`
    /// names.
    fn pin(label: &str, n: u8) -> ActivePin {
        ActivePin {
            label: label.to_string(),
            hash: format!("{n:02x}").repeat(32),
        }
    }

    /// A dependency set root carrying one library in its `lib`
    /// directory, laid out like a mounted or extracted build.
    fn set_lib(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        fs::create_dir_all(dir.join("lib")).unwrap();
        fs::write(dir.join("lib").join(name), bytes).unwrap();
        dir.to_path_buf()
    }

    /// The store directory the identity of `pins` records under.
    fn sel_dir(store: &Path, pins: &[ActivePin]) -> PathBuf {
        store.join(selection_dir(SelectionIdentity::new(pins).unwrap().key()).unwrap())
    }

    /// Pin a file's mtime to a fixed past instant, so a later write of
    /// the same bytes still reads as one.
    fn backdate(path: &Path) -> std::time::SystemTime {
        let times = std::fs::FileTimes::new()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000));
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(times)
            .unwrap();
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000)
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
        assert!(r.farm.as_ref().is_some_and(|f| f.starts_with(&w.store)));
        assert_eq!(w.farm_link("libfoo.so.1"), Some(host_foo));
        // The driver itself is not bundled, so it can only come from the
        // host, and it must be reachable without a host directory on the
        // search path.
        assert_eq!(w.farm_link("libGL.so.1"), Some(host_gl));
        assert!(r.host_interp.is_none());
        assert!(r.incomparable.is_empty());
    }

    /// A report has to show each verdict and where the farm would be,
    /// and must not leave a farm or a recorded decision behind.
    #[test]
    fn deciding_names_every_verdict_and_writes_nothing() {
        let w = World::new();
        let host_gl = w.host(
            "libGL.so.1",
            &driver_needing(&["libfoo.so.1", "libbar.so.1"]),
        );
        let host_foo = w.host("libfoo.so.1", &versioned(&["FOO_1.0", "FOO_1.1"]));
        w.host("libbar.so.1", &versioned(&["BAR_1.0"]));
        w.bundle("libfoo.so.1", &versioned(&["FOO_1.0"]));
        w.bundle("libbar.so.1", &versioned(&["BAR_1.0", "BAR_1.1"]));
        let icd_dir = w.icd_dir.to_string_lossy().into_owned();
        let req = Request {
            pkg_root: &w.pkg,
            lib_dirs: &["lib"],
            plugin_dirs: &[],
            policy: HostLibsPolicy::Auto,
            store: &w.store,
            ld_cache: &w.cache,
            icd_dirs: &[icd_dir.as_str()],
            extra_root: None,
            set_roots: &[],
            host_libc: true,
            loader_store: w.loader_store(),
            nsswitch: &w.nsswitch,
        };

        let d = decide(&req);
        assert_eq!(
            d.compared,
            vec![
                ("libGL.so.1".to_string(), None),
                ("libbar.so.1".to_string(), Some(Choice::Bundle)),
                ("libfoo.so.1".to_string(), Some(Choice::Host)),
            ]
        );
        assert_eq!(d.winners.get("libfoo.so.1"), Some(&host_foo));
        assert_eq!(d.winners.get("libGL.so.1"), Some(&host_gl));
        assert!(!w.store.exists(), "deciding wrote the store");

        let r = resolve_selected(&req, &[]);
        assert_eq!(
            r.farm, d.farm,
            "the farm a launch builds is the one reported"
        );
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
        let closure = HostIndex::load(&w.cache, &[], None).driver_closure();
        for name in ["libvulkan.so.1", "libdrm.so.2", "libz.so.1", "libgone.so.0"] {
            assert!(closure.contains(name), "{name} missing from {closure:?}");
        }
        assert!(!closure.contains("libunrelated.so.1"));
    }

    /// A synthetic host loader carrying the default directory table and
    /// cache path a real one has, so it can be closed.
    fn closable_loader() -> Vec<u8> {
        let mut image = versioned(&["GLIBC_2.2.5"]);
        image.extend_from_slice(b"\0/etc/ld.so.cache\0/lib64/\0/usr/lib64/\0");
        image
    }

    /// A host libc newer than the bundled one, built for the loader at
    /// `host_ld`, with the bundle carrying the older pair.
    fn newer_host_glibc(w: &World, host_ld: &Path) -> PathBuf {
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.2.5"]));
        w.bundle("ld-linux-x86-64.so.2", &versioned(&["GLIBC_2.2.5"]));
        w.host(
            "libc.so.6",
            &shared_object_with_interp(
                &[(0, "GLIBC_2.2.5"), (0, "GLIBC_2.34")],
                &[],
                Some(host_ld.to_str().unwrap()),
            ),
        )
    }

    #[test]
    fn a_newer_host_glibc_brings_its_loader_and_family() {
        let w = World::new();
        let host_ld = w.host("ld-linux-x86-64.so.2", &closable_loader());
        let host_libc = newer_host_glibc(&w, &host_ld);
        let host_libm = w.host("libm.so.6", &versioned(&["GLIBC_2.2.5", "GLIBC_2.34"]));
        w.bundle("libm.so.6", &versioned(&["GLIBC_2.2.5"]));
        w.bundle("libother.so.1", &versioned(&["OTHER_1"]));

        let r = w.resolve(HostLibsPolicy::Auto);
        let loader = r.host_interp.clone().expect("the host's loader runs");
        assert_eq!(
            loader.path,
            w.loader_root
                .join("loaders")
                .join(loader.hash.to_hex().as_str())
                .join("ld-linux-x86-64.so.2")
        );
        for dir in [&w.loader_root, &w.loader_root.join("loaders")] {
            assert!(crate::cache_layout::is_safe_owned_dir(dir, w.uid));
        }
        assert_eq!(w.farm_link("libc.so.6"), Some(host_libc));
        assert_eq!(w.farm_link("libm.so.6"), Some(host_libm));
        assert_eq!(
            w.farm_link("ld-linux-x86-64.so.2"),
            Some(loader.path.clone()),
            "the shadowed bundled loader path shows the closed copy"
        );
        assert!(w.farm_link("libother.so.1").is_none());

        let mut closed = closable_loader();
        ldso::scrub_glibc_loader(&mut closed).unwrap();
        assert_eq!(fs::read(&loader.path).unwrap(), closed);
        assert_eq!(loader.hash, blake3::hash(&closed));
        loader.verify().unwrap();
        assert_eq!(
            fs::read(&host_ld).unwrap(),
            closable_loader(),
            "the host's loader is never written"
        );

        let decided = w.decide(HostLibsPolicy::Auto);
        assert_eq!(decided.farm, r.farm, "the report names the launch's farm");
        assert_eq!(decided.host_interp.unwrap().source, host_ld);
    }

    /// A counterpart module that demands a version of the loader
    /// soname is weighed against the interpreter the pair runs, not
    /// the cache's entry for that soname: the interpreter is the copy
    /// the farm links for the module to load.
    #[test]
    fn a_module_demands_versions_of_the_interpreter_not_the_cache_entry() {
        let w = World::new();
        // The cache names a loader whose build defines no 2.34; the
        // interpreter the host libc names does.
        w.host("ld-linux-x86-64.so.2", &versioned(&["GLIBC_2.2.5"]));
        let interp_dir = w.host_lib.parent().unwrap().join("interp");
        fs::create_dir_all(&interp_dir).unwrap();
        let interp = interp_dir.join("ld-linux-x86-64.so.2");
        let mut image = versioned(&["GLIBC_2.2.5", "GLIBC_2.34"]);
        image.extend_from_slice(b"\0/etc/ld.so.cache\0/lib64/\0/usr/lib64/\0");
        fs::write(&interp, &image).unwrap();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.2.5"]));
        w.host(
            "libc.so.6",
            &shared_object_with_interp(
                &[(0, "GLIBC_2.2.5"), (0, "GLIBC_2.34")],
                &[],
                Some(interp.to_str().unwrap()),
            ),
        );
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(
            &module,
            module_bytes(
                0,
                &["ld-linux-x86-64.so.2"],
                &[("ld-linux-x86-64.so.2", &[(0, "GLIBC_2.34")])],
            ),
        )
        .unwrap();

        let decided = w.decide(HostLibsPolicy::Auto);
        assert_eq!(
            decided.winners.get("ld-linux-x86-64.so.2"),
            Some(&interp),
            "the interpreter, not the cache's entry, supplies the soname"
        );
        assert_eq!(decided.admitted, vec![module.clone()]);

        let r = w.resolve(HostLibsPolicy::Auto);
        assert_eq!(r.admitted, vec![module]);
        let loader = r.host_interp.expect("the host's loader runs");
        assert_eq!(w.farm_link("ld-linux-x86-64.so.2"), Some(loader.path));
    }

    /// A counterpart module that demands a loader soname other than
    /// the interpreter's own file name is still weighed against the
    /// interpreter the pair runs, and the farm links the closed copy
    /// under the demanded name: the cache's distinct entry for that
    /// soname supplies neither the versions nor the launch.
    #[test]
    fn a_module_demands_a_loader_soname_the_interpreter_supplies() {
        let w = World::new();
        // The cache's entry for the demanded soname defines no 2.34;
        // the interpreter the host libc names, under another name, does.
        w.host("ld-linux-x86-64.so.2", &versioned(&["GLIBC_2.2.5"]));
        let interp_dir = w.host_lib.parent().unwrap().join("interp");
        fs::create_dir_all(&interp_dir).unwrap();
        let interp = interp_dir.join("ld64.so.2");
        let mut image = versioned(&["GLIBC_2.2.5", "GLIBC_2.34"]);
        image.extend_from_slice(b"\0/etc/ld.so.cache\0/lib64/\0/usr/lib64/\0");
        fs::write(&interp, &image).unwrap();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.2.5"]));
        w.host(
            "libc.so.6",
            &shared_object_with_interp(
                &[(0, "GLIBC_2.2.5"), (0, "GLIBC_2.34")],
                &[],
                Some(interp.to_str().unwrap()),
            ),
        );
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(
            &module,
            module_bytes(
                0,
                &["ld-linux-x86-64.so.2"],
                &[("ld-linux-x86-64.so.2", &[(0, "GLIBC_2.34")])],
            ),
        )
        .unwrap();

        let decided = w.decide(HostLibsPolicy::Auto);
        assert_eq!(
            decided.winners.get("ld-linux-x86-64.so.2"),
            Some(&interp),
            "the demanded soname names the interpreter the pair runs"
        );
        assert_eq!(decided.admitted, vec![module.clone()]);

        let r = w.resolve(HostLibsPolicy::Auto);
        assert_eq!(r.admitted, vec![module]);
        let loader = r.host_interp.expect("the host's loader runs");
        assert_eq!(w.farm_link("ld-linux-x86-64.so.2"), Some(loader.path));
    }

    /// A loader whose image names no default directory the scrub knows
    /// cannot be shown closed, so the bundled pair runs and the reason
    /// is there for the report.
    #[test]
    fn a_host_loader_that_cannot_be_closed_keeps_the_bundled_pair() {
        let w = World::new();
        let host_ld = w.host("ld-linux-x86-64.so.2", &versioned(&["GLIBC_2.2.5"]));
        newer_host_glibc(&w, &host_ld);

        let r = w.resolve(HostLibsPolicy::Always);
        assert!(r.host_interp.is_none());
        assert!(w.farm_link("libc.so.6").is_none());
        assert!(w.farm_link("ld-linux-x86-64.so.2").is_none());

        let decided = w.decide(HostLibsPolicy::Always);
        assert!(decided.host_interp.is_none());
        let (source, why) = decided.host_interp_refused.unwrap();
        assert_eq!(source, host_ld);
        assert!(why.contains("no default search directory"), "{why}");

        fs::write(&host_ld, b"not an ELF image").unwrap();
        let (_, why) = w
            .decide(HostLibsPolicy::Always)
            .host_interp_refused
            .unwrap();
        assert!(why.contains("not an ELF object"), "{why}");
    }

    /// Without a place a loader copy can run from, the host's pair is not
    /// taken, and a record made while there was one is not served.
    #[test]
    fn a_refused_loader_store_keeps_the_bundled_pair() {
        let w = World::new();
        let host_ld = w.host("ld-linux-x86-64.so.2", &closable_loader());
        newer_host_glibc(&w, &host_ld);
        assert!(w.resolve(HostLibsPolicy::Always).host_interp.is_some());

        let icd_dir = w.icd_dir.to_string_lossy().into_owned();
        let req = Request {
            pkg_root: &w.pkg,
            lib_dirs: &["lib"],
            plugin_dirs: &[],
            policy: HostLibsPolicy::Always,
            store: &w.store,
            ld_cache: &w.cache,
            icd_dirs: &[icd_dir.as_str()],
            extra_root: None,
            set_roots: &[],
            host_libc: true,
            loader_store: LoaderStore::Refused("the loader copy's filesystem is mounted noexec"),
            nsswitch: &w.nsswitch,
        };
        let r = resolve_selected(&req, &[]);
        assert!(r.host_interp.is_none());
        assert!(r.farm.is_none_or(|farm| !farm.join("libc.so.6").exists()));
        let (source, why) = decide(&req).host_interp_refused.unwrap();
        assert_eq!(source, host_ld);
        assert_eq!(why, "the loader copy's filesystem is mounted noexec");
    }

    /// A copy that cannot be written after all takes the host's pair away
    /// but nothing else: the driver still comes from the host, and since
    /// the fallback is not recorded, a later launch tries the copy again.
    #[test]
    fn a_copy_that_cannot_be_written_keeps_the_rest_of_the_decision() {
        let w = World::new();
        let host_ld = w.host("ld-linux-x86-64.so.2", &closable_loader());
        newer_host_glibc(&w, &host_ld);
        let host_gl = w.host("libGL.so.1", &driver_needing(&[]));
        fs::create_dir_all(&w.loader_root).unwrap();
        fs::write(w.loader_root.join("loaders"), b"in the way").unwrap();

        let r = w.resolve(HostLibsPolicy::Auto);
        assert!(r.host_interp.is_none());
        assert_eq!(w.farm_link("libGL.so.1"), Some(host_gl));
        assert!(w.farm_link("libc.so.6").is_none());
        assert!(w.farm_link("ld-linux-x86-64.so.2").is_none());
        assert!(!sel_dir(&w.store, &[]).join(DECISION_FILE).exists());

        fs::remove_file(w.loader_root.join("loaders")).unwrap();
        let r = w.resolve(HostLibsPolicy::Auto);
        assert!(r.host_interp.is_some());
        assert!(w.farm_link("libc.so.6").is_some());
    }

    /// Two decisions closing the same loader share one copy.
    #[test]
    fn one_copy_serves_every_decision_of_a_loader() {
        let w = World::new();
        let host_ld = w.host("ld-linux-x86-64.so.2", &closable_loader());
        newer_host_glibc(&w, &host_ld);
        let first = w.resolve(HostLibsPolicy::Auto).host_interp.unwrap();
        let second = w.resolve(HostLibsPolicy::Always).host_interp.unwrap();
        assert_eq!(first, second);
        let copies = fs::read_dir(w.loader_root.join("loaders")).unwrap().count();
        assert_eq!(copies, 1);
    }

    /// The host libc opens its name service modules and `libgcc_s` by
    /// name, and the closed loader finds them only in the farm. Each
    /// service the configuration names is chosen when the bundle lacks
    /// it, with what it needs, and none is chosen beside the bundled libc.
    #[test]
    fn the_host_libc_gets_the_modules_it_opens_by_name() {
        let w = World::new();
        let host_ld = w.host("ld-linux-x86-64.so.2", &closable_loader());
        newer_host_glibc(&w, &host_ld);
        let sss = w.host("libnss_sss.so.2", &driver_needing(&["libsssdep.so.1"]));
        let dep = w.host("libsssdep.so.1", &versioned(&[]));
        let gcc = w.host("libgcc_s.so.1", &versioned(&["GCC_3.0"]));
        w.host("libnss_unnamed.so.2", &versioned(&[]));
        fs::write(
            &w.nsswitch,
            "# comment: unnamed\npasswd: files sss [NOTFOUND=return] systemd\n\
             group:    sss\nbogus line\nhosts: Bad.Name dns\n",
        )
        .unwrap();

        w.resolve(HostLibsPolicy::Auto);
        assert_eq!(w.farm_link("libnss_sss.so.2"), Some(sss));
        assert_eq!(w.farm_link("libsssdep.so.1"), Some(dep));
        assert_eq!(w.farm_link("libgcc_s.so.1"), Some(gcc));
        assert!(w.farm_link("libnss_unnamed.so.2").is_none());
        assert_eq!(
            host_libc_opens(&w.nsswitch),
            [
                "libgcc_s.so.1",
                "libnss_dns.so.2",
                "libnss_files.so.2",
                "libnss_sss.so.2",
                "libnss_systemd.so.2",
            ]
        );

        fs::write(&host_ld, versioned(&["GLIBC_2.2.5"])).unwrap();
        w.write_cache();
        w.resolve(HostLibsPolicy::Auto);
        assert!(w.farm_link("libnss_sss.so.2").is_none());
        assert!(w.farm_link("libgcc_s.so.1").is_none());
    }

    /// A configuration larger than any real one names nothing, and only
    /// its first lines are read.
    #[test]
    fn the_name_service_configuration_is_read_bounded() {
        let w = World::new();
        assert_eq!(host_libc_opens(&w.nsswitch), ["libgcc_s.so.1"]);
        let mut text = "passwd: files\n".repeat(NSSWITCH_MAX_LINES);
        text.push_str("group: late\n");
        fs::write(&w.nsswitch, &text).unwrap();
        assert!(!host_libc_opens(&w.nsswitch).contains(&"libnss_late.so.2".to_string()));
        let huge = "passwd: big\n".repeat(NSSWITCH_MAX_BYTES as usize / 4);
        fs::write(&w.nsswitch, huge).unwrap();
        assert_eq!(host_libc_opens(&w.nsswitch), ["libgcc_s.so.1"]);
    }

    /// Where loader copies can be kept is part of the host: a refusal
    /// recorded while the store was unusable is not served once it is.
    #[test]
    fn the_loader_store_keys_the_decision() {
        let w = World::new();
        let stamp = |store| {
            fingerprint(
                &w.cache,
                &[],
                &[],
                HostLibsPolicy::Auto,
                None,
                true,
                &w.nsswitch,
                store,
            )
        };
        let refused = stamp(LoaderStore::Refused("noexec"));
        assert_ne!(refused, stamp(w.loader_store()));
        assert_eq!(refused, stamp(LoaderStore::Refused("another reason")));
        let elsewhere = w.loader_root.join("elsewhere");
        let moved = stamp(LoaderStore::Root {
            root: &elsewhere,
            uid: w.uid,
        });
        assert_ne!(moved, stamp(w.loader_store()));
    }

    /// A changed name service configuration is a changed host.
    #[test]
    fn the_name_service_configuration_keys_the_decision() {
        let w = World::new();
        let before = fingerprint(
            &w.cache,
            &[],
            &[],
            HostLibsPolicy::Auto,
            None,
            true,
            &w.nsswitch,
            w.loader_store(),
        );
        fs::write(&w.nsswitch, "passwd: files\n").unwrap();
        let after = fingerprint(
            &w.cache,
            &[],
            &[],
            HostLibsPolicy::Auto,
            None,
            true,
            &w.nsswitch,
            w.loader_store(),
        );
        assert_ne!(before, after);
    }

    /// The counterpart directories of the plugin directories the package
    /// carries join the decision: the compiled-in absolute path itself,
    /// and the directory's path under every library directory the cache
    /// names, a multiarch host's included.
    #[test]
    fn a_plugin_directory_keys_the_decision_on_its_counterparts() {
        let w = World::new();
        let multiarch = w.host_lib.join("x86_64-linux-gnu");
        let alsa = multiarch.join("alsa-lib");
        fs::create_dir_all(&alsa).unwrap();
        let named = multiarch.join("libasound.so.2");
        fs::write(&named, b"elf").unwrap();
        let refs: Vec<String> = vec![named.to_string_lossy().into_owned()];
        let str_refs: Vec<&str> = refs.iter().map(String::as_str).collect();
        fs::write(&w.cache, new_cache(&str_refs)).unwrap();
        let absolute = w.host_lib.join("alsa-lib");
        fs::create_dir_all(&absolute).unwrap();
        let carried = vec![PluginDir {
            relative: "lib/alsa-lib".to_owned(),
            lib_relative: "alsa-lib".to_owned(),
            absolutes: vec![absolute.to_string_lossy().into_owned()],
        }];
        let with = fingerprint(
            &w.cache,
            &carried,
            &[],
            HostLibsPolicy::Auto,
            None,
            true,
            &w.nsswitch,
            w.loader_store(),
        );
        assert!(
            with.contains(&format!("\x1f{}", alsa.display())),
            "the multiarch counterpart is stamped: {with}"
        );
        assert!(
            with.contains(&format!("\x1f{}", absolute.display())),
            "the same absolute path is stamped: {with}"
        );
        let plain = fingerprint(
            &w.cache,
            &[],
            &[],
            HostLibsPolicy::Auto,
            None,
            true,
            &w.nsswitch,
            w.loader_store(),
        );
        assert!(!plain.contains(alsa.to_string_lossy().as_ref()));
        assert_ne!(plain, with);
    }

    /// A counterpart that does not exist is stamped nowhere: neither a
    /// compiled-in absolute that is missing, nor a library-directory
    /// join the host has no such directory under.
    #[test]
    fn an_absent_counterpart_is_not_stamped() {
        let w = World::new();
        w.host("libasound.so.2", &versioned(&[]));
        let carried = vec![PluginDir {
            relative: "lib/alsa-lib".to_owned(),
            lib_relative: "alsa-lib".to_owned(),
            absolutes: vec![w.host_lib.join("nowhere").to_string_lossy().into_owned()],
        }];
        let stamped = fingerprint(
            &w.cache,
            &carried,
            &[],
            HostLibsPolicy::Auto,
            None,
            true,
            &w.nsswitch,
            w.loader_store(),
        );
        assert!(!stamped.contains("nowhere"));
        assert!(!stamped.contains("alsa-lib"));
    }

    /// A module arriving in, or leaving, a counterpart directory changes
    /// the fingerprint, so the decision it keyed recomputes.
    #[test]
    fn a_module_added_to_or_removed_from_a_counterpart_rekeys_the_decision() {
        let w = World::new();
        w.host("libasound.so.2", &versioned(&[]));
        let alsa = w.host_lib.join("alsa-lib");
        fs::create_dir_all(&alsa).unwrap();
        let carried = vec![PluginDir {
            relative: "lib/alsa-lib".to_owned(),
            lib_relative: "alsa-lib".to_owned(),
            absolutes: Vec::new(),
        }];
        let stamp = || {
            fingerprint(
                &w.cache,
                &carried,
                &[],
                HostLibsPolicy::Auto,
                None,
                true,
                &w.nsswitch,
                w.loader_store(),
            )
        };
        let before = stamp();
        let module = alsa.join("libasound_module_pcm_pipe.so");
        fs::write(&module, b"elf").unwrap();
        // Set explicitly: a directory's mtime can be coarser than the
        // time between the two writes.
        fs::File::open(&alsa)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000))
            .unwrap();
        let added = stamp();
        assert_ne!(before, added);
        fs::remove_file(&module).unwrap();
        fs::File::open(&alsa)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000))
            .unwrap();
        let removed = stamp();
        assert_ne!(added, removed);
    }

    /// A launch after a host module arrives in a counterpart directory
    /// recomputes the decision, and its farm is a new one.
    #[test]
    fn a_new_module_in_a_counterpart_resolves_afresh() {
        let w = World::new();
        let host_ld = w.host("ld-linux-x86-64.so.2", &closable_loader());
        newer_host_glibc(&w, &host_ld);
        let alsa = w.host_lib.join("alsa-lib");
        fs::create_dir_all(&alsa).unwrap();
        w.carry(vec![PluginDir {
            relative: "lib/alsa-lib".to_owned(),
            lib_relative: "alsa-lib".to_owned(),
            absolutes: Vec::new(),
        }]);
        let first = w.resolve_full(HostLibsPolicy::Always, None, true);
        let farm1 = first.farm.clone().expect("a farm the launch built");
        fs::write(alsa.join("libasound_module_pcm_pipe.so"), b"elf").unwrap();
        fs::File::open(&alsa)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000))
            .unwrap();
        let second = w.resolve_full(HostLibsPolicy::Always, None, true);
        let farm2 = second.farm.clone().expect("a farm the launch built");
        assert_ne!(farm1, farm2);
        fs::remove_file(alsa.join("libasound_module_pcm_pipe.so")).unwrap();
        fs::File::open(&alsa)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000))
            .unwrap();
        let third = w.resolve_full(HostLibsPolicy::Always, None, true);
        assert_ne!(farm2, third.farm.unwrap());
    }

    /// A recorded decision is served only while its loader copy matches
    /// the digest it recorded. A damaged copy is a miss, and deciding
    /// again writes the copy back rather than running what is there.
    #[test]
    fn a_damaged_loader_copy_is_never_served() {
        let w = World::new();
        let host_ld = w.host("ld-linux-x86-64.so.2", &closable_loader());
        newer_host_glibc(&w, &host_ld);
        let first = w.resolve(HostLibsPolicy::Auto).host_interp.unwrap();

        fs::write(&first.path, b"\x7fELF tampered").unwrap();
        assert!(first.verify().is_err());
        let again = w.resolve(HostLibsPolicy::Auto).host_interp.unwrap();
        assert_eq!(again, first);
        again.verify().unwrap();

        fs::remove_file(&first.path).unwrap();
        let again = w.resolve(HostLibsPolicy::Auto).host_interp.unwrap();
        again.verify().unwrap();
    }

    /// A copy is trusted by its digest only where nobody else could have
    /// put it or could swap it: a copy behind a loosened directory, one
    /// owned by another uid, or a link to an identical file is never
    /// served, and deciding again writes a private copy back.
    #[test]
    fn a_copy_outside_private_directories_is_never_served() {
        use std::os::unix::fs::PermissionsExt;
        let w = World::new();
        let host_ld = w.host("ld-linux-x86-64.so.2", &closable_loader());
        newer_host_glibc(&w, &host_ld);
        let copy = w.resolve(HostLibsPolicy::Auto).host_interp.unwrap();
        let identity = SelectionIdentity::new(&[]).unwrap();
        let fingerprint = fingerprint(
            &w.cache,
            &[],
            &[w.icd_dir.to_str().unwrap()],
            HostLibsPolicy::Auto,
            None,
            true,
            &w.nsswitch,
            w.loader_store(),
        );
        let dir = sel_dir(&w.store, &[]);
        let load = || load_selection(&dir, &identity, &fingerprint, &w.store, w.loader_store());
        assert!(load().is_some());
        assert!(!copy_is_private(
            &w.loader_root,
            &copy,
            w.uid.wrapping_add(1)
        ));

        let copy_dir = copy.path.parent().unwrap();
        fs::set_permissions(copy_dir, fs::Permissions::from_mode(0o770)).unwrap();
        assert!(load().is_none(), "a group-writable directory");
        let again = w.resolve(HostLibsPolicy::Auto).host_interp.unwrap();
        assert!(copy_is_private(&w.loader_root, &again, w.uid));

        let elsewhere = w.loader_root.join("planted");
        fs::copy(&copy.path, &elsewhere).unwrap();
        fs::remove_file(&copy.path).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &copy.path).unwrap();
        copy.verify().unwrap();
        assert!(load().is_none(), "a link to an identical file");
        let again = w.resolve(HostLibsPolicy::Auto).host_interp.unwrap();
        assert!(!fs::symlink_metadata(&again.path).unwrap().is_symlink());
        assert!(load().is_some());
    }

    /// A record naming a digest other than its copy's, or a copy name
    /// that leaves the farm, is a miss decided afresh.
    #[test]
    fn a_record_with_a_foreign_loader_is_a_miss() {
        let w = World::new();
        let host_ld = w.host("ld-linux-x86-64.so.2", &closable_loader());
        newer_host_glibc(&w, &host_ld);
        let first = w.resolve(HostLibsPolicy::Auto).host_interp.unwrap();
        let record = sel_dir(&w.store, &[]).join(DECISION_FILE);
        let text = fs::read_to_string(&record).unwrap();
        let line = format!("interp {}\tld-linux-x86-64.so.2", first.hash.to_hex());
        assert!(text.contains(&line), "{text}");

        let identity = SelectionIdentity::new(&[]).unwrap();
        let fingerprint = fingerprint(
            &w.cache,
            &[],
            &[w.icd_dir.to_str().unwrap()],
            HostLibsPolicy::Auto,
            None,
            true,
            &w.nsswitch,
            w.loader_store(),
        );
        let dir = sel_dir(&w.store, &[]);
        assert!(
            load_selection(&dir, &identity, &fingerprint, &w.store, w.loader_store()).is_some()
        );
        for forged in [
            format!(
                "interp {}\tld-linux-x86-64.so.2",
                blake3::hash(b"other").to_hex()
            ),
            format!("interp {}\t..", first.hash.to_hex()),
            format!("interp {}", first.hash.to_hex()),
            "interp nothex\tld-linux-x86-64.so.2".to_string(),
        ] {
            fs::write(&record, text.replace(&line, &forged)).unwrap();
            assert!(
                load_selection(&dir, &identity, &fingerprint, &w.store, w.loader_store()).is_none(),
                "{forged}"
            );
        }
        fs::write(&record, text.replace(&line, &format!("{line}\n{line}"))).unwrap();
        assert!(
            load_selection(&dir, &identity, &fingerprint, &w.store, w.loader_store()).is_none()
        );
    }

    /// Under the closed host loader nothing is found outside the farm, so
    /// a glibc family member a host driver needs and the bundle lacks is
    /// chosen with the pair.
    #[test]
    fn a_driver_gets_the_family_member_the_bundle_lacks() {
        let w = World::new();
        let host_ld = w.host("ld-linux-x86-64.so.2", &closable_loader());
        newer_host_glibc(&w, &host_ld);
        let host_dl = w.host("libdl.so.2", &versioned(&["GLIBC_2.2.5"]));
        w.host("libGL.so.1", &driver_needing(&["libdl.so.2"]));

        w.resolve(HostLibsPolicy::Auto);
        assert_eq!(w.farm_link("libdl.so.2"), Some(host_dl));

        fs::write(&host_ld, versioned(&["GLIBC_2.2.5"])).unwrap();
        w.write_cache();
        w.resolve(HostLibsPolicy::Auto);
        assert!(
            w.farm_link("libdl.so.2").is_none(),
            "beside the bundled libc no host family member is taken"
        );
    }

    /// Under the host loader the bundled executables' run path still finds
    /// the bundled libc, so a launch that cannot put the host's copy in its
    /// place must keep the bundled pair rather than crash at start.
    #[test]
    fn a_launch_that_cannot_shadow_keeps_the_bundled_glibc() {
        let w = World::new();
        let host_ld = w.host("ld-linux-x86-64.so.2", &closable_loader());
        newer_host_glibc(&w, &host_ld);

        let r = w.resolve_full(HostLibsPolicy::Auto, None, false);
        assert!(r.host_interp.is_none());
        assert!(w.farm_link("libc.so.6").is_none());

        // The decision recorded for it is not the one a launch that can
        // shadow gets.
        let r = w.resolve_full(HostLibsPolicy::Auto, None, true);
        assert!(r.host_interp.is_some());
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

    /// The 64-bit bytes of a counterpart module of `machine` that
    /// needs `needed` and demands `requirements`, matching the
    /// fixture-built bundled pair's machine.
    fn module_bytes(
        machine: u16,
        needed: &[&str],
        requirements: &[(&str, &[(u16, &str)])],
    ) -> Vec<u8> {
        fixture::facts_elf(true, machine, None, needed, requirements).bytes
    }

    /// A named pipe at `path`, for tests that must not block on it.
    fn make_fifo(path: &Path) {
        let ok = Command::new("mkfifo")
            .arg(path)
            .status()
            .expect("mkfifo(1) must exist")
            .success();
        assert!(ok, "mkfifo failed for {}", path.display());
    }

    /// A package carrying `lib/alsa-lib`, whose host counterpart is the
    /// `alsa-lib` directory beside the host's libraries.
    fn carry_counterpart(w: &World) -> PathBuf {
        let dir = w.host_lib.join("alsa-lib");
        fs::create_dir_all(&dir).unwrap();
        w.carry(vec![PluginDir {
            relative: "lib/alsa-lib".to_string(),
            lib_relative: "alsa-lib".to_string(),
            absolutes: Vec::new(),
        }]);
        dir
    }

    /// The refusal recorded for `module`.
    fn refusal_for<'a>(refused: &'a [ModuleRefusal], module: &Path) -> &'a ModuleRefusal {
        refused
            .iter()
            .find(|r| r.module == module)
            .expect("the module is refused")
    }

    /// A counterpart module the checks admit brings its host
    /// dependencies into the decision, as chosen host libraries.
    #[test]
    fn a_counterpart_module_admits_with_its_host_dependencies() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(&module, module_bytes(0, &["libdep.so.2"], &[])).unwrap();

        let decision = w.decide(HostLibsPolicy::Auto);

        assert_eq!(decision.admitted, vec![module]);
        assert!(decision.refused.is_empty());
        assert_eq!(
            decision.winners.get("libdep.so.2"),
            Some(&w.host_lib.join("libdep.so.2"))
        );
        assert!(
            decision
                .compared
                .iter()
                .any(|(n, c)| n == "libdep.so.2" && c.is_none()),
            "a soname the bundle lacks is named as host-only"
        );
    }

    /// A module built for another machine is refused on itself and
    /// nothing it needs is offered.
    #[test]
    fn a_module_for_another_architecture_is_refused_with_its_dependencies() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(&module, module_bytes(183, &["libdep.so.2"], &[])).unwrap();

        let decision = w.decide(HostLibsPolicy::Auto);

        assert!(decision.admitted.is_empty());
        assert_eq!(decision.refused.len(), 1);
        assert_eq!(decision.refused[0].module, module);
        assert_eq!(decision.refused[0].object, module);
        assert!(decision.refused[0].reason.contains("architecture"));
        assert!(!decision.winners.contains_key("libdep.so.2"));
        assert!(!decision.compared.iter().any(|(n, _)| n == "libdep.so.2"));
    }

    /// A musl-linked module is refused: it must not pull its libc into
    /// the process.
    #[test]
    fn a_musl_module_is_refused() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(&module, module_bytes(0, &["libc.musl-x86_64.so.1"], &[])).unwrap();

        let decision = w.decide(HostLibsPolicy::Auto);

        assert!(decision.admitted.is_empty());
        assert_eq!(decision.refused.len(), 1);
        assert!(decision.refused[0].reason.contains("glibc family"));
    }

    /// A module is weighed by the versions it demands: one whose demand
    /// the copies here define is admitted beside one whose demand
    /// nothing defines, which is refused naming the version.
    #[test]
    fn a_module_is_admitted_or_refused_by_the_versions_it_demands() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        let dir = carry_counterpart(&w);
        let fitting = dir.join("libfits.so");
        fs::write(
            &fitting,
            module_bytes(0, &["libc.so.6"], &[("libc.so.6", &[(0, "GLIBC_2.32")])]),
        )
        .unwrap();
        let demanding = dir.join("libgreedy.so");
        fs::write(
            &demanding,
            module_bytes(0, &["libc.so.6"], &[("libc.so.6", &[(0, "GLIBC_2.99")])]),
        )
        .unwrap();

        let decision = w.decide(HostLibsPolicy::Auto);

        assert_eq!(decision.admitted, vec![fitting]);
        assert_eq!(decision.refused.len(), 1);
        assert_eq!(decision.refused[0].module, demanding);
        assert!(decision.refused[0].reason.contains("GLIBC_2.99"));
    }

    /// A module whose dependency nothing here supplies is refused, and
    /// an unreadable file that names itself a module is refused too,
    /// with a readable neighbour still admitted.
    #[test]
    fn an_unsupplied_dependency_or_unreadable_module_is_refused() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        let dir = carry_counterpart(&w);
        let missing = dir.join("libwants.so");
        fs::write(&missing, module_bytes(0, &["libnothere.so.9"], &[])).unwrap();
        let junk = dir.join("libjunk.so");
        fs::write(&junk, b"not elf").unwrap();
        let plain = dir.join("libplain.so");
        fs::write(&plain, module_bytes(0, &[], &[])).unwrap();

        let decision = w.decide(HostLibsPolicy::Auto);

        assert_eq!(decision.admitted, vec![plain]);
        assert_eq!(decision.refused.len(), 2);
        let reasons: Vec<(PathBuf, String)> = decision
            .refused
            .iter()
            .map(|r| (r.module.clone(), r.reason.clone()))
            .collect();
        assert!(
            reasons
                .iter()
                .any(|(m, r)| m == &missing && r.contains("nothing supplies")),
            "{reasons:?}"
        );
        assert!(
            reasons
                .iter()
                .any(|(m, r)| m == &junk && r.contains("read")),
            "{reasons:?}"
        );
    }

    /// A module carrying an old-style `DT_RPATH` is refused: the
    /// loader searches that path ahead of the farm, so an unchecked
    /// copy could answer a soname the decision chose.
    #[test]
    fn a_module_with_an_old_search_path_is_refused() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        // DT_RPATH is tag 15.
        fs::write(
            &module,
            fixture::facts_elf_with(true, 0, None, &[], &[], &[(15, 0)]).bytes,
        )
        .unwrap();

        let decision = w.decide(HostLibsPolicy::Auto);
        assert!(decision.admitted.is_empty());
        let refusal = refusal_for(&decision.refused, &module);
        assert_eq!(refusal.object, module);
        assert!(refusal.reason.contains("search path"), "{refusal:?}");
    }

    /// A runpath supersedes an old-style search path: the loader ignores
    /// the rpath of an object that also carries a runpath, so the module
    /// is admitted like one with a runpath alone.
    #[test]
    fn a_runpath_supersedes_an_old_search_path() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        // DT_RPATH is tag 15, DT_RUNPATH is 29.
        fs::write(
            &module,
            fixture::facts_elf_with(true, 0, None, &[], &[], &[(15, 0), (29, 0)]).bytes,
        )
        .unwrap();

        let decision = w.decide(HostLibsPolicy::Auto);
        assert_eq!(decision.admitted, vec![module]);
    }

    /// A module whose need names the loader the bundled pair runs is
    /// admitted without any copy on disk: the process runs that loader
    /// already, and glibc answers the demand from the mapped copy.
    #[test]
    fn a_module_naming_the_loader_is_admitted_without_a_copy() {
        let w = World::new();
        w.bundle(
            "libc.so.6",
            &shared_object_with_interp(&[(0, "GLIBC_2.32")], &[], Some("ld-linux-x86-64.so.2")),
        );
        w.host("libextra.so.1", &versioned(&["EXTRA_1"]));
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(&module, module_bytes(0, &["ld-linux-x86-64.so.2"], &[])).unwrap();

        let decision = w.decide(HostLibsPolicy::Auto);
        assert_eq!(decision.admitted, vec![module]);
        assert!(!decision.winners.contains_key("ld-linux-x86-64.so.2"));
    }

    /// A module naming a glibc loader the pair does not run is refused:
    /// the mapped loader answers only its own soname, and a foreign
    /// loader name would have to load from disk.
    #[test]
    fn a_foreign_loader_name_is_refused_under_the_bundled_pair() {
        let w = World::new();
        w.bundle(
            "libc.so.6",
            &shared_object_with_interp(&[(0, "GLIBC_2.32")], &[], Some("ld-linux-x86-64.so.2")),
        );
        w.host("libextra.so.1", &versioned(&["EXTRA_1"]));
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(&module, module_bytes(0, &["ld.so.1"], &[])).unwrap();

        let decision = w.decide(HostLibsPolicy::Auto);
        assert!(decision.admitted.is_empty());
        let refusal = refusal_for(&decision.refused, &module);
        assert!(refusal.reason.contains("nothing supplies"), "{refusal:?}");
    }

    /// A stray file holding the farm's path is replaced: the launch
    /// after it serves a real farm again instead of wedging forever.
    #[test]
    fn a_stray_file_at_the_farm_path_is_replaced() {
        let w = World::new();
        w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(&module, module_bytes(0, &["libdep.so.2"], &[])).unwrap();
        fs::create_dir_all(&w.store).unwrap();

        w.resolve(HostLibsPolicy::Auto);
        let farm = w.last_farm.borrow().clone().expect("a farm");
        fs::remove_dir_all(&farm).unwrap();
        fs::write(&farm, b"stray").unwrap();

        let second = w.resolve(HostLibsPolicy::Auto);
        let rebuilt = second
            .farm
            .expect("the farm is rebuilt over the stray file");
        assert_eq!(rebuilt, farm);
        assert!(rebuilt.join("libdep.so.2").exists());
    }

    /// A module naming a filter object is refused: a filter loads its
    /// objects outside the closure, so nothing vouches for them.
    #[test]
    fn a_module_with_a_filter_object_is_refused() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        // DT_FILTER is 0x7fffffff.
        fs::write(
            &module,
            fixture::facts_elf_with(true, 0, None, &[], &[], &[(0x7fffffff, 0)]).bytes,
        )
        .unwrap();

        let decision = w.decide(HostLibsPolicy::Auto);
        assert!(decision.admitted.is_empty());
        let refusal = refusal_for(&decision.refused, &module);
        assert!(refusal.reason.contains("filter"), "{refusal:?}");
    }

    /// A module whose requirement hash does not match its name cannot
    /// be read: the loader would reject the demand at launch, so
    /// admission refuses instead of presenting a module that fails.
    #[test]
    fn a_module_whose_requirement_hash_lies_is_refused() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        let mut bytes = module_bytes(0, &["libdep.so.2"], &[("libdep.so.2", &[(0, "DEP_1.0")])]);
        let good = verdef::elf_hash("DEP_1.0").to_le_bytes();
        let at = bytes
            .windows(4)
            .position(|window| window == good)
            .expect("the fixture carries the requirement hash");
        bytes[at..at + 4].copy_from_slice(&(verdef::elf_hash("DEP_1.0") ^ 0x10).to_le_bytes());
        fs::write(&module, bytes).unwrap();

        let decision = w.decide(HostLibsPolicy::Auto);
        assert!(decision.admitted.is_empty());
        let refusal = refusal_for(&decision.refused, &module);
        assert!(refusal.reason.contains("read"), "{refusal:?}");
        assert!(!decision.winners.contains_key("libdep.so.2"));
    }

    /// A cache entry that is a named pipe cannot hang admission: the
    /// walk finds nothing that supplies the dependency and refuses.
    #[test]
    fn a_fifo_cache_entry_is_refused_without_hanging() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(&module, module_bytes(0, &["libdep.so.2"], &[])).unwrap();
        let dep = w.host_lib.join("libdep.so.2");
        fs::remove_file(&dep).unwrap();
        make_fifo(&dep);

        let decision = w.decide(HostLibsPolicy::Auto);
        assert!(decision.admitted.is_empty());
        let refusal = refusal_for(&decision.refused, &module);
        assert!(refusal.reason.contains("read"), "{refusal:?}");
        assert_eq!(refusal.object, dep);
        assert!(!decision.winners.contains_key("libdep.so.2"));
    }

    /// A loader source that is a named pipe cannot hang the close: the
    /// host pair is refused as unreadable and the bundled pair runs.
    #[test]
    fn a_loader_that_is_a_fifo_is_refused() {
        let w = World::new();
        let host_ld = w.host("ld-linux-x86-64.so.2", &closable_loader());
        newer_host_glibc(&w, &host_ld);
        fs::remove_file(&host_ld).unwrap();
        make_fifo(&host_ld);

        let decided = w.decide(HostLibsPolicy::Always);
        assert!(decided.host_interp.is_none());
        let (source, why) = decided.host_interp_refused.unwrap();
        assert_eq!(source, host_ld);
        assert!(why.contains("read"), "{why}");
    }

    /// A module needing a glibc family member the bundle lacks takes it
    /// from the host when the host's pair won: it joins the farm like
    /// the rest of its family, or the closed loader could not find it.
    #[test]
    fn a_module_takes_an_unbundled_family_soname_through_the_host_pair() {
        let w = World::new();
        let host_ld = w.host("ld-linux-x86-64.so.2", &closable_loader());
        newer_host_glibc(&w, &host_ld);
        w.host("libm.so.6", &versioned(&["GLIBC_2.2.5", "GLIBC_2.34"]));
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(&module, module_bytes(0, &["libm.so.6"], &[])).unwrap();

        let decision = w.decide(HostLibsPolicy::Auto);
        assert_eq!(decision.admitted, vec![module.clone()]);
        assert!(decision.refused.is_empty());
        assert_eq!(
            decision.winners.get("libm.so.6"),
            Some(&w.host_lib.join("libm.so.6"))
        );

        let r = w.resolve(HostLibsPolicy::Auto);
        assert!(r.host_interp.is_some(), "the host's pair runs");
        assert_eq!(w.farm_link("libm.so.6"), Some(w.host_lib.join("libm.so.6")));
    }

    /// Under the bundled pair a module needing a family member the
    /// bundle lacks is refused, even though the host carries one:
    /// nothing outside the farm can be found, and the family is not
    /// farmed piecemeal.
    #[test]
    fn an_unbundled_family_soname_refuses_a_module_under_the_bundled_pair() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        w.host("libm.so.6", &versioned(&["GLIBC_2.2.5"]));
        w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(&module, module_bytes(0, &["libm.so.6"], &[])).unwrap();

        let decision = w.decide(HostLibsPolicy::Auto);

        assert!(decision.admitted.is_empty());
        let refusal = refusal_for(&decision.refused, &module);
        assert_eq!(refusal.object, module);
        assert!(refusal.reason.contains("libm.so.6"));
        assert!(!decision.winners.contains_key("libm.so.6"));
    }

    /// A soname the bundle carries satisfies a module and stays out of
    /// the decision, unlike a family member only the host's pair can
    /// supply, which joins the farm: the bundle keeps what it has.
    #[test]
    fn a_bundled_soname_satisfies_a_module_without_a_new_candidate() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        w.bundle("libown.so.1", &versioned(&["OWN_1.0"]));
        w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(&module, module_bytes(0, &["libown.so.1"], &[])).unwrap();

        let decision = w.decide(HostLibsPolicy::Auto);

        assert_eq!(decision.admitted, vec![module]);
        assert!(!decision.winners.contains_key("libown.so.1"));
        assert!(!decision.compared.iter().any(|(n, _)| n == "libown.so.1"));
    }

    /// A package without a bundled pair to check against admits
    /// nothing: every module is refused, sorted by path.
    #[test]
    fn a_package_without_a_bundled_pair_refuses_every_module() {
        let w = World::new();
        w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        fs::create_dir_all(w.host_lib.join("alsa-lib/xsub")).unwrap();
        let z = w.host_lib.join("alsa-lib/z.so");
        let a = w.host_lib.join("alsa-lib/xsub/a.so");
        fs::write(&z, module_bytes(0, &["libdep.so.2"], &[])).unwrap();
        fs::write(&a, module_bytes(0, &["libdep.so.2"], &[])).unwrap();
        w.carry(vec![
            PluginDir {
                relative: "lib/alsa-lib".to_string(),
                lib_relative: "alsa-lib".to_string(),
                absolutes: Vec::new(),
            },
            PluginDir {
                relative: "lib/alsa-lib/xsub".to_string(),
                lib_relative: "alsa-lib/xsub".to_string(),
                absolutes: Vec::new(),
            },
        ]);

        let decision = w.decide(HostLibsPolicy::Auto);

        assert!(decision.admitted.is_empty());
        let refused: Vec<PathBuf> = decision.refused.iter().map(|r| r.module.clone()).collect();
        assert_eq!(refused, vec![a, z]);
    }

    /// A module added to a counterpart directory between two decisions
    /// is noticed: the second decision considers it.
    #[test]
    fn a_module_added_between_launches_is_considered() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        let dir = carry_counterpart(&w);

        assert!(w.decide(HostLibsPolicy::Auto).admitted.is_empty());
        let module = dir.join("libplug.so");
        fs::write(&module, module_bytes(0, &["libdep.so.2"], &[])).unwrap();

        assert_eq!(w.decide(HostLibsPolicy::Auto).admitted, vec![module]);
    }

    /// A supplied family dependency is walked like any host library:
    /// what it needs in turn joins the farm, so the closed loader can
    /// load the whole chain.
    #[test]
    fn a_family_dependency_s_own_needs_reach_the_farm_too() {
        let w = World::new();
        let host_ld = w.host("ld-linux-x86-64.so.2", &closable_loader());
        newer_host_glibc(&w, &host_ld);
        w.host("libm.so.6", &versioned(&["GLIBC_2.2.5", "GLIBC_2.34"]));
        w.host("libmvec.so.1", &driver_needing(&["libm.so.6"]));
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(&module, module_bytes(0, &["libmvec.so.1"], &[])).unwrap();

        let decision = w.decide(HostLibsPolicy::Auto);
        assert_eq!(decision.admitted, vec![module]);
        assert!(decision.refused.is_empty());

        w.resolve(HostLibsPolicy::Auto);
        assert_eq!(
            w.farm_link("libmvec.so.1"),
            Some(w.host_lib.join("libmvec.so.1"))
        );
        assert_eq!(w.farm_link("libm.so.6"), Some(w.host_lib.join("libm.so.6")));
    }

    /// A module is weighed against the copy that would load: under the
    /// host's pair its newer libc defines a demand the bundled copy
    /// cannot, and the module is admitted.
    #[test]
    fn a_module_demands_what_the_pair_in_use_defines() {
        let w = World::new();
        let host_ld = w.host("ld-linux-x86-64.so.2", &closable_loader());
        newer_host_glibc(&w, &host_ld);
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(
            &module,
            module_bytes(0, &["libc.so.6"], &[("libc.so.6", &[(0, "GLIBC_2.34")])]),
        )
        .unwrap();

        let decision = w.decide(HostLibsPolicy::Auto);

        assert_eq!(decision.admitted, vec![module]);
        assert!(decision.refused.is_empty());
    }

    /// A refusal names the object of the closure that failed, not just
    /// the module: a dependency whose facts cannot be read is named
    /// itself.
    #[test]
    fn a_refusal_names_the_dependency_that_failed() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        w.host("libdep.so.2", b"not elf");
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(&module, module_bytes(0, &["libdep.so.2"], &[])).unwrap();

        let decision = w.decide(HostLibsPolicy::Auto);

        assert!(decision.admitted.is_empty());
        let refusal = refusal_for(&decision.refused, &module);
        assert_eq!(refusal.object, w.host_lib.join("libdep.so.2"));
        assert!(refusal.reason.contains("read"), "{}", refusal.reason);
    }

    /// Under the host's pair a family demand is weighed against the
    /// host's copy, the one that loads: a demand only the bundled copy
    /// defines is refused.
    #[test]
    fn a_family_demand_only_the_bundled_copy_defines_is_refused() {
        let w = World::new();
        let host_ld = w.host("ld-linux-x86-64.so.2", &closable_loader());
        newer_host_glibc(&w, &host_ld);
        w.host("libm.so.6", &versioned(&["GLIBC_2.2.5", "GLIBC_2.34"]));
        w.bundle("libm.so.6", &versioned(&["GLIBC_2.2.5", "GLIBC_2.99"]));
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(
            &module,
            module_bytes(0, &["libm.so.6"], &[("libm.so.6", &[(0, "GLIBC_2.99")])]),
        )
        .unwrap();

        let decision = w.decide(HostLibsPolicy::Auto);

        assert!(decision.admitted.is_empty());
        let refusal = refusal_for(&decision.refused, &module);
        assert!(refusal.reason.contains("GLIBC_2.99"), "{}", refusal.reason);
    }

    /// The farm links the host's family copy under its pair, so a host
    /// copy whose facts cannot be read supplies nothing, whatever the
    /// bundled copy would define.
    #[test]
    fn an_unreadable_host_family_copy_supplies_nothing() {
        let w = World::new();
        let host_ld = w.host("ld-linux-x86-64.so.2", &closable_loader());
        newer_host_glibc(&w, &host_ld);
        w.bundle("libm.so.6", &versioned(&["GLIBC_2.2.5", "GLIBC_2.99"]));
        w.host("libm.so.6", b"not elf");
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(
            &module,
            module_bytes(0, &["libm.so.6"], &[("libm.so.6", &[(0, "GLIBC_2.99")])]),
        )
        .unwrap();

        let decision = w.decide(HostLibsPolicy::Auto);

        assert!(decision.admitted.is_empty());
        let refusal = refusal_for(&decision.refused, &module);
        assert!(
            refusal.reason.contains("nothing supplies"),
            "{}",
            refusal.reason
        );
    }

    /// Under the host's pair a bundled family soname loads from the
    /// host's copy, so that copy's closure is walked: what it needs
    /// reaches the farm, and a copy that fails a check refuses the
    /// module.
    #[test]
    fn a_bundled_family_soname_is_walked_through_its_host_copy() {
        let w = World::new();
        let host_ld = w.host("ld-linux-x86-64.so.2", &closable_loader());
        newer_host_glibc(&w, &host_ld);
        w.bundle("libm.so.6", &versioned(&["GLIBC_2.2.5"]));
        w.host("libm.so.6", &driver_needing(&["libhidden.so.1"]));
        let hidden = w.host("libhidden.so.1", &versioned(&["HID_1"]));
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(&module, module_bytes(0, &["libm.so.6"], &[])).unwrap();

        let decision = w.decide(HostLibsPolicy::Auto);
        assert_eq!(decision.admitted, vec![module.clone()]);
        assert!(decision.refused.is_empty());

        w.resolve(HostLibsPolicy::Auto);
        assert_eq!(w.farm_link("libhidden.so.1"), Some(hidden));

        // A selected copy that fails a check refuses the module.
        fs::write(w.host_lib.join("libm.so.6"), module_bytes(183, &[], &[])).unwrap();
        w.write_cache();
        let decision = w.decide(HostLibsPolicy::Auto);
        assert!(decision.admitted.is_empty());
        let refusal = refusal_for(&decision.refused, &module);
        assert!(
            refusal.reason.contains("architecture"),
            "{}",
            refusal.reason
        );
    }

    /// A refused module repaired in place, at the same size, is weighed
    /// again: the fingerprint stamps the counterpart files themselves,
    /// so the record cannot serve a stale refusal.
    #[test]
    fn a_repaired_module_is_weighed_again() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(
            &module,
            module_bytes(0, &["libc.so.6"], &[("libc.so.6", &[(0, "GLIBC_2.99")])]),
        )
        .unwrap();

        let first = w.resolve(HostLibsPolicy::Auto);
        assert!(first.admitted.is_empty());

        // Same size, new demand: the repair is invisible to the
        // directory stamps.
        fs::write(
            &module,
            module_bytes(0, &["libc.so.6"], &[("libc.so.6", &[(0, "GLIBC_2.32")])]),
        )
        .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&module)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_001))
            .unwrap();

        assert_eq!(w.resolve(HostLibsPolicy::Auto).admitted, vec![module]);
    }

    /// Under `always` the decision compares the bundle's own sonames,
    /// so a module needing one walks the copy the decision won: what
    /// the winning copy demands reaches the farm.
    #[test]
    fn a_module_walks_the_copy_the_decision_won() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        w.bundle("libfoo.so.1", &versioned(&["FOO_1.0"]));
        w.host(
            "libfoo.so.1",
            &shared_object(&[(0, "FOO_1.0"), (0, "FOO_2.0")], &["libhelp.so.1"]),
        );
        let help = w.host("libhelp.so.1", &versioned(&["HELP_1"]));
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(&module, module_bytes(0, &["libfoo.so.1"], &[])).unwrap();

        let decision = w.decide(HostLibsPolicy::Always);
        assert_eq!(decision.admitted, vec![module]);
        assert!(decision.refused.is_empty());

        w.resolve(HostLibsPolicy::Always);
        assert_eq!(w.farm_link("libhelp.so.1"), Some(help));
    }

    /// The same rule for versions: a demand is weighed against the copy
    /// the decision won, so a version only the winning host copy
    /// defines admits the module.
    #[test]
    fn a_module_weighs_the_copy_the_decision_won() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        w.bundle("libfoo.so.1", &versioned(&["FOO_1.0"]));
        w.host(
            "libfoo.so.1",
            &shared_object(&[(0, "FOO_1.0"), (0, "FOO_2.0")], &[]),
        );
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(
            &module,
            module_bytes(0, &["libfoo.so.1"], &[("libfoo.so.1", &[(0, "FOO_2.0")])]),
        )
        .unwrap();

        let decision = w.decide(HostLibsPolicy::Always);

        assert_eq!(decision.admitted, vec![module]);
        assert!(decision.refused.is_empty());
    }

    /// Admissions and refusals come back sorted by path even when the
    /// counterpart directories' scan order differs, so the launch's
    /// view and the record's canonical order agree. A nested directory
    /// sorts before its parent's own modules under path ordering.
    #[test]
    fn a_decision_sorts_modules_across_counterparts() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        fs::create_dir_all(w.host_lib.join("alsa-lib/xsub")).unwrap();
        let z = w.host_lib.join("alsa-lib/z.so");
        let a = w.host_lib.join("alsa-lib/xsub/a.so");
        w.carry(vec![
            PluginDir {
                relative: "lib/alsa-lib".to_string(),
                lib_relative: "alsa-lib".to_string(),
                absolutes: Vec::new(),
            },
            PluginDir {
                relative: "lib/alsa-lib/xsub".to_string(),
                lib_relative: "alsa-lib/xsub".to_string(),
                absolutes: Vec::new(),
            },
        ]);

        fs::write(&z, module_bytes(0, &[], &[])).unwrap();
        fs::write(&a, module_bytes(0, &[], &[])).unwrap();
        let decision = w.decide(HostLibsPolicy::Auto);
        assert_eq!(decision.admitted, vec![a.clone(), z.clone()]);

        fs::write(&z, module_bytes(183, &[], &[])).unwrap();
        fs::write(&a, module_bytes(183, &[], &[])).unwrap();
        let decision = w.decide(HostLibsPolicy::Auto);
        assert!(decision.admitted.is_empty());
        let refused: Vec<PathBuf> = decision.refused.iter().map(|r| r.module.clone()).collect();
        assert_eq!(refused, vec![a, z]);
    }

    /// A counterpart module rewritten beneath a restored modification
    /// time is weighed again: the stamp carries the change time, which
    /// a rewrite always moves and a tool cannot pin back.
    #[test]
    fn a_module_rewritten_beneath_a_restored_mtime_is_weighed_again() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(
            &module,
            module_bytes(0, &["libc.so.6"], &[("libc.so.6", &[(0, "GLIBC_2.32")])]),
        )
        .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&module)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_002))
            .unwrap();

        let first = w.resolve(HostLibsPolicy::Auto);
        assert_eq!(first.admitted, vec![module.clone()]);

        // Same size, the modification time pinned back to the same
        // instant, a new demand: only the change time tells the rewrite
        // apart. The sleep keeps the two writes in different kernel
        // timestamp ticks, which coarse inode clocks otherwise share.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(
            &module,
            module_bytes(0, &["libc.so.6"], &[("libc.so.6", &[(0, "GLIBC_2.99")])]),
        )
        .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&module)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_002))
            .unwrap();

        let second = w.resolve(HostLibsPolicy::Auto);
        assert!(second.admitted.is_empty());
    }

    /// A record must also carry exactly one surviving stat for every
    /// winner it chose: a winner without its stat, a duplicated winner
    /// stat and a stat naming an undeclared winner are each a miss, or
    /// an in-place swap of an unvouched winner would be served.
    #[test]
    fn a_record_without_one_stat_per_winner_is_never_served() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        let host_dep = w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(&module, module_bytes(0, &["libdep.so.2"], &[])).unwrap();
        let pins = [pin("gtk", 1)];
        let identity = SelectionIdentity::new(&pins).unwrap();
        let icd_dir = w.icd_dir.to_string_lossy().into_owned();
        let plugin_dirs = w.plugin_dirs.borrow();
        let fingerprint = fingerprint(
            &w.cache,
            &plugin_dirs,
            &[icd_dir.as_str()],
            HostLibsPolicy::Auto,
            None,
            true,
            &w.nsswitch,
            w.loader_store(),
        );
        w.resolve_selection(HostLibsPolicy::Auto, &[], &pins);
        let record = sel_dir(&w.store, &pins).join(DECISION_FILE);
        let good = fs::read_to_string(&record).unwrap();
        let winner_entry = format!("stat libdep.so.2\t{}", stat_line(&host_dep));
        let served = |text: &str| {
            fs::write(&record, text).unwrap();
            load_selection(
                sel_dir(&w.store, &pins).as_path(),
                &identity,
                &fingerprint,
                &w.store,
                w.loader_store(),
            )
        };
        let miss = |text: String, why: &str| {
            assert_eq!(served(&text), None, "{why}");
        };

        assert!(served(&good).is_some(), "the intact record is served");

        miss(
            good.replace(&format!("{winner_entry}\n"), ""),
            "a winner without its stat",
        );
        miss(
            good.replace(&winner_entry, &format!("{winner_entry}\n{winner_entry}")),
            "a duplicated winner stat",
        );
        miss(
            good.replace(
                &winner_entry,
                &format!("stat libown.so.9\t{}", stat_line(&host_dep)),
            ),
            "a stat naming an undeclared winner",
        );

        // A repeated `host` line, even one placed after the winner's
        // stat verified the first path, must not adopt the repeat: its
        // path was never statted, so the record would vouch a winner
        // nobody checked.
        let alt = w.store.join("alt-copy.so.2");
        fs::write(&alt, b"not the copy the host state vouches").unwrap();
        let forged = good.replace(
            &winner_entry,
            &format!("{winner_entry}\nhost libdep.so.2\t{}", alt.display()),
        );
        assert!(
            parse_decision(
                &forged,
                SELECTION_RECORD_HEADER,
                &identity.identity_lines(),
                &fingerprint
            )
            .is_none(),
            "a repeated host line moves the winner past its stat"
        );
    }

    /// A recorded decision carries its admissions: a launch served
    /// from the store sees the same modules a fresh decision admitted.
    #[test]
    fn the_record_carries_the_admitted_modules() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(&module, module_bytes(0, &["libdep.so.2"], &[])).unwrap();
        fs::create_dir_all(&w.store).unwrap();

        let first = w.resolve(HostLibsPolicy::Auto);
        assert_eq!(first.admitted, vec![module.clone()]);
        let served = w.resolve(HostLibsPolicy::Auto);
        assert_eq!(served.admitted, vec![module], "served from the record");
        assert_eq!(served.farm, first.farm);
        assert_eq!(served.host_interp, first.host_interp);
    }

    /// A record with no winners at all serves too: a module admitted
    /// on bundled sonames alone round trips without a farm or a
    /// loader.
    #[test]
    fn a_record_without_winners_still_serves_its_admissions() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        w.host("libextra.so.1", &versioned(&["EXTRA_1"]));
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(&module, module_bytes(0, &["libc.so.6"], &[])).unwrap();
        fs::create_dir_all(&w.store).unwrap();

        let first = w.resolve(HostLibsPolicy::Auto);
        assert_eq!(first.admitted, vec![module.clone()]);
        assert!(first.farm.is_none());
        let served = w.resolve(HostLibsPolicy::Auto);
        assert_eq!(served.admitted, vec![module], "served from the record");
        assert_eq!(served.farm, None);
        assert!(served.host_interp.is_none());
    }

    /// A record must carry exactly one surviving stat for every module
    /// it admits: a duplicated module line, a module without its stat, a
    /// stat naming an undeclared module, a duplicated stat, a stat
    /// forged to `missing`, a record truncated before its admissions
    /// and a module that has since vanished are each a miss, while the
    /// intact record is served.
    #[test]
    fn a_record_without_one_stat_per_module_is_never_served() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(&module, module_bytes(0, &["libdep.so.2"], &[])).unwrap();
        let pins = [pin("gtk", 1)];
        let identity = SelectionIdentity::new(&pins).unwrap();
        let icd_dir = w.icd_dir.to_string_lossy().into_owned();
        let plugin_dirs = w.plugin_dirs.borrow();
        let fingerprint = fingerprint(
            &w.cache,
            &plugin_dirs,
            &[icd_dir.as_str()],
            HostLibsPolicy::Auto,
            None,
            true,
            &w.nsswitch,
            w.loader_store(),
        );
        w.resolve_selection(HostLibsPolicy::Auto, &[], &pins);
        let record = sel_dir(&w.store, &pins).join(DECISION_FILE);
        let good = fs::read_to_string(&record).unwrap();
        let module_entry = format!("module {}", module.display());
        let stat_prefix = format!("module-stat {}\t", module.display());
        let served = |text: &str| {
            fs::write(&record, text).unwrap();
            load_selection(
                sel_dir(&w.store, &pins).as_path(),
                &identity,
                &fingerprint,
                &w.store,
                w.loader_store(),
            )
        };
        let miss = |text: String, why: &str| {
            assert_eq!(served(&text), None, "{why}");
        };

        assert!(served(&good).is_some(), "the intact record is served");

        let stat_at = good
            .find(&stat_prefix)
            .expect("the record stats the module");
        let stat_end = good[stat_at..].find('\n').unwrap() + stat_at;
        let recorded_stat = good[stat_at + stat_prefix.len()..stat_end].to_string();
        let stat_entry = format!("{stat_prefix}{recorded_stat}");

        miss(
            good.replace(&module_entry, &format!("{module_entry}\n{module_entry}")),
            "a duplicated module line",
        );
        miss(
            good.replace(&format!("{stat_entry}\n"), ""),
            "a module without its stat",
        );
        miss(
            good.replacen(
                &stat_entry,
                &format!(
                    "module-stat {}\t{}",
                    w.host_lib.join("libdep.so.2").display(),
                    stat_line(&w.host_lib.join("libdep.so.2"))
                ),
                1,
            ),
            "a stat naming an undeclared module",
        );
        miss(
            good.replace(&stat_entry, &format!("{stat_entry}\n{stat_entry}")),
            "a duplicated module stat",
        );
        miss(
            good.replace(&stat_entry, &format!("{stat_prefix}missing")),
            "a stat forged to missing",
        );
        miss(
            good[..good
                .find("admitted ")
                .expect("the record declares its count")]
                .to_string(),
            "a record truncated before its admissions",
        );

        fs::remove_file(&module).unwrap();
        miss(good.clone(), "a module that has since vanished");
    }

    #[test]
    fn the_decision_is_recorded_and_reused_until_the_host_changes() {
        let w = World::new();
        w.host("libGL.so.1", &driver_needing(&["libfoo.so.1"]));
        let host_foo = w.host("libfoo.so.1", &versioned(&["FOO_1.0", "FOO_1.1"]));
        w.bundle("libfoo.so.1", &versioned(&["FOO_1.0"]));
        let first = w.resolve(HostLibsPolicy::Auto);
        assert_eq!(w.farm_link("libfoo.so.1"), Some(host_foo.clone()));

        // Nothing changed: the recorded decision is reused as it stands.
        assert_eq!(w.resolve(HostLibsPolicy::Auto), first);
        assert_eq!(w.farm_link("libfoo.so.1"), Some(host_foo.clone()));

        // Rewriting the file in place changes neither the cache nor the
        // directory, but the decision records the winner's own stat, so
        // this is re-judged. The rewritten copy now matches the bundle's
        // version exactly, and the bundle keeps its own.
        fs::write(&host_foo, versioned(&["FOO_1.0"])).unwrap();
        w.resolve(HostLibsPolicy::Auto);
        assert!(w.farm_link("libfoo.so.1").is_none());

        // A new entry in the directory is a host change; the fresh
        // comparison agrees with what the re-judgement found.
        fs::write(w.host_lib.join("libnew.so.1"), versioned(&[])).unwrap();
        bump_mtime(&w.host_lib);
        w.resolve(HostLibsPolicy::Auto);
        assert!(w.farm_link("libfoo.so.1").is_none());
    }

    #[test]
    fn an_in_place_same_size_swap_is_not_served_stale() {
        let w = World::new();
        w.host("libGL.so.1", &driver_needing(&["libfoo.so.1"]));
        let host_foo = w.host("libfoo.so.1", &versioned(&["FOO_1.0", "FOO_1.1"]));
        w.bundle("libfoo.so.1", &versioned(&["FOO_1.0"]));
        w.resolve(HostLibsPolicy::Auto);
        assert_eq!(w.farm_link("libfoo.so.1"), Some(host_foo.clone()));

        // Same byte count, different versions: neither side's version set
        // contains the other's, so the recorded decision is void. The
        // directory stamps see nothing here. The mtime is pinned to a
        // distinct instant because two writes in one kernel timestamp
        // tick share ctime; a real swap always lands later.
        fs::write(&host_foo, versioned(&["FOO_0.9", "FOO_1.1"])).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&host_foo)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000))
            .unwrap();
        w.resolve(HostLibsPolicy::Auto);
        assert!(
            w.farm_link("libfoo.so.1").is_none(),
            "a swapped winner must be re-judged, not served from the record"
        );
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

    /// An image that has `libGL.so.1` and not `libEGL.so.1`, which is
    /// what an X server pulls in, is not a stack for a package whose
    /// toolkit links `libEGL`. Counting it as one leaves the launch to
    /// fail on the missing half instead of fetching the pinned build.
    #[test]
    fn a_partial_host_stack_does_not_count() {
        let w = World::new();
        w.host("libGL.so.1", &shared_object(&[(1, "GL_1")], &[]));
        w.bundle(
            "libQtGui.so.6",
            &shared_object(&[(1, "Qt_6")], &["libEGL.so.1"]),
        );
        assert_eq!(
            w.gl(),
            Gl::Missing,
            "the host has no libEGL, which the bundle asks for"
        );
        w.host("libEGL.so.1", &shared_object(&[(1, "EGL_1")], &[]));
        assert_eq!(w.gl(), Gl::Host, "both halves present");
    }

    #[test]
    fn the_gl_situation_is_bundled_host_missing_or_absent() {
        let w = World::new();
        w.host("libz.so.1", &shared_object(&[(1, "Z_1")], &[]));
        assert_eq!(w.gl(), Gl::Absent, "no GL entry point anywhere");
        w.bundle(
            "libgtk-3.so.0",
            &shared_object(&[(1, "Gtk_3")], &["libepoxy.so.0"]),
        );
        assert_eq!(
            w.gl(),
            Gl::Absent,
            "a toolkit that opens GL by name links none"
        );
        w.bundle(
            "libQtGui.so.6",
            &shared_object(&[(1, "Qt_6")], &["libEGL.so.1"]),
        );
        assert_eq!(w.gl(), Gl::Missing, "the bundle links libEGL");
        w.host("libEGL.so.1", &shared_object(&[(1, "EGL_1")], &[]));
        assert_eq!(w.gl(), Gl::Host);
        w.bundle("libGL.so.1", &shared_object(&[(1, "GL_1")], &[]));
        assert_eq!(w.gl(), Gl::Bundled, "the bundle's own stack comes first");
    }

    #[test]
    fn the_launched_binary_names_gl_like_a_library() {
        let w = World::new();
        w.host("libz.so.1", &shared_object(&[(1, "Z_1")], &[]));
        let app = w.pkg.join("bin/app");
        fs::create_dir_all(app.parent().unwrap()).unwrap();
        fs::write(&app, shared_object(&[(1, "App_1")], &["libGL.so.1"])).unwrap();
        assert_eq!(
            gl_situation(&w.pkg, &["lib"], &[], None, &w.cache, &[]),
            Gl::Absent,
            "a binary the launch does not run asks for nothing"
        );
        assert_eq!(
            gl_situation(&w.pkg, &["lib"], &[], Some(&app), &w.cache, &[]),
            Gl::Missing,
            "the launched binary links libGL"
        );
    }

    #[test]
    fn an_extra_root_supplies_what_the_host_lacks() {
        let w = World::new();
        w.host("libz.so.1", &shared_object(&[(1, "Z_1")], &[]));
        w.bundle("libc.so.6", &shared_object(&[(2, "GLIBC_2.30")], &[]));
        let root = temp_dir("glbuild");
        fs::create_dir_all(root.join("lib")).unwrap();
        fs::create_dir_all(root.join("share/vulkan/icd.d")).unwrap();
        let gl = root.join("lib/libGL.so.1");
        fs::write(&gl, shared_object(&[(1, "GL_1")], &["libdrm.so.2"])).unwrap();
        let drm = root.join("lib/libdrm.so.2");
        fs::write(&drm, shared_object(&[(1, "DRM_1")], &[])).unwrap();
        let icd = root.join("lib/libvulkan_fake.so");
        fs::write(&icd, shared_object(&[(1, "ICD_1")], &[])).unwrap();
        let vendor = root.join("lib/libEGL_fake.so.0");
        fs::write(&vendor, shared_object(&[(1, "VENDOR_1")], &["libdrm.so.2"])).unwrap();
        fs::write(
            root.join("share/vulkan/icd.d/fake.json"),
            format!("{{\"ICD\": {{\"library_path\": \"{}\"}}}}", icd.display()),
        )
        .unwrap();

        let without = w.resolve(HostLibsPolicy::Auto);
        assert_eq!(without.farm, None, "nothing to take from a host without GL");

        let with = w.resolve_with(HostLibsPolicy::Auto, Some(&root));
        assert_eq!(w.farm_link("libGL.so.1"), Some(gl));
        assert_eq!(
            w.farm_link("libdrm.so.2"),
            Some(drm),
            "the build's closure follows"
        );
        assert_eq!(
            w.farm_link("libvulkan_fake.so"),
            Some(icd),
            "ICD files seed the walk"
        );
        assert_eq!(
            w.farm_link("libEGL_fake.so.0"),
            Some(vendor),
            "a vendor library nothing names is farmed by its name"
        );
        assert!(with.farm.is_some());
        assert_eq!(
            with.host_interp, None,
            "the extra root never supplies a loader"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_host_change_leaves_the_old_farm_for_running_instances() {
        let w = World::new();
        let host_gl = w.host("libGL.so.1", &versioned(&["GL_1.0", "GL_2.0"]));
        w.bundle("libGL.so.1", &versioned(&["GL_1.0"]));
        w.resolve(HostLibsPolicy::Auto);
        let old_farm = w.last_farm.borrow().clone().expect("a farm");
        assert_eq!(
            fs::read_link(old_farm.join("libGL.so.1")).ok(),
            Some(host_gl)
        );

        // The host changes, so the next launch decides afresh. An instance
        // started before is still loading through the old farm.
        w.host("libnew.so.1", &versioned(&[]));
        w.resolve(HostLibsPolicy::Auto);
        let new_farm = w.last_farm.borrow().clone().expect("a farm");
        assert_ne!(old_farm, new_farm);
        assert!(
            old_farm.join("libGL.so.1").exists(),
            "the old farm was torn down under a running instance"
        );
        assert!(new_farm.join("libGL.so.1").exists());
    }

    /// A farm left behind by an earlier launch is reused only when its
    /// links still name exactly the chosen copies: a relinked or extra
    /// entry in it is rebuilt over, never handed to a launch.
    #[test]
    fn a_damaged_or_grown_farm_is_rebuilt_for_the_next_launch() {
        let w = World::new();
        let host_dep = w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(&module, module_bytes(0, &["libdep.so.2"], &[])).unwrap();
        fs::create_dir_all(&w.store).unwrap();

        w.resolve(HostLibsPolicy::Auto);
        let farm = w.last_farm.borrow().clone().expect("a farm");
        let planted = w.store.join("planted.so.1");
        fs::write(&planted, b"planted").unwrap();

        // A winner link bent to another file is rebuilt.
        let link = farm.join("libdep.so.2");
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&planted, &link).unwrap();
        w.resolve(HostLibsPolicy::Auto);
        assert_eq!(
            fs::read_link(&link).ok(),
            Some(host_dep),
            "the tampered link is re-aimed at the chosen copy"
        );

        // An entry beyond the winners is dropped with the rebuild.
        std::os::unix::fs::symlink(&planted, farm.join("extra.so.9")).unwrap();
        w.resolve(HostLibsPolicy::Auto);
        assert!(
            farm.join("extra.so.9").symlink_metadata().is_err(),
            "the extra farm entry does not survive the rebuild"
        );
    }

    #[test]
    fn a_failed_materialize_is_not_recorded() {
        use std::os::unix::fs::PermissionsExt;

        let w = World::new();
        w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        let dir = carry_counterpart(&w);
        let module = dir.join("libplug.so");
        fs::write(&module, module_bytes(0, &["libdep.so.2"], &[])).unwrap();
        fs::create_dir_all(&w.store).unwrap();

        w.resolve(HostLibsPolicy::Auto);
        let farm = w.last_farm.borrow().clone().expect("a farm");
        let record = sel_dir(&w.store, &[]).join("decision");
        let before = fs::metadata(&record).unwrap().modified().unwrap();

        // A farm directory nobody can empty wedges the rebuild, and the
        // failed materialize must not overwrite the served record.
        fs::remove_dir_all(&farm).unwrap();
        fs::create_dir(&farm).unwrap();
        fs::write(farm.join("stuck.so"), b"stuck").unwrap();
        fs::set_permissions(&farm, fs::Permissions::from_mode(0o500)).unwrap();

        let r = w.resolve(HostLibsPolicy::Auto);
        assert!(r.farm.is_none(), "the wedged farm is not served");
        let after = fs::metadata(&record).unwrap().modified().unwrap();
        assert_eq!(
            before, after,
            "the failed materialize leaves the recorded decision alone"
        );
        assert_eq!(
            fs::read_link(farm.join("libdep.so.2")).ok(),
            None,
            "the wedged occupant is left for the next launch to retry"
        );
    }

    #[test]
    fn a_view_links_bundled_members_then_admitted_host_ones() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        let counterpart = carry_counterpart(&w);
        let module = counterpart.join("libtype.so");
        fs::write(&module, module_bytes(0, &["libdep.so.2"], &[])).unwrap();
        let shadowed = counterpart.join("libcarried.so");
        fs::write(&shadowed, module_bytes(0, &["libdep.so.2"], &[])).unwrap();
        let bundled = w.pkg.join("lib/alsa-lib");
        fs::create_dir_all(&bundled).unwrap();
        let carried = bundled.join("libcarried.so");
        fs::write(&carried, module_bytes(0, &["libdep.so.2"], &[])).unwrap();
        fs::write(bundled.join("plugin.conf"), b"config").unwrap();
        fs::create_dir_all(&w.store).unwrap();

        let r = w.resolve(HostLibsPolicy::Auto);

        assert_eq!(r.plugin_views.len(), 1, "one view, for the carried record");
        let view = &r.plugin_views[0];
        assert_eq!(view.relative, "lib/alsa-lib");
        let entries: BTreeMap<String, PathBuf> = fs::read_dir(&view.dir)
            .unwrap()
            .flatten()
            .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
            .collect();
        assert_eq!(
            entries.len(),
            3,
            "two bundled members and the one host member the package lacks"
        );
        assert_eq!(
            fs::read_link(view.dir.join("libcarried.so")).ok(),
            Some(carried),
            "a name the package carries links the package's own copy"
        );
        assert_eq!(
            fs::read_link(view.dir.join("plugin.conf")).ok(),
            Some(bundled.join("plugin.conf")),
            "bundled data members ride along"
        );
        assert_eq!(
            fs::read_link(view.dir.join("libtype.so")).ok(),
            Some(module),
            "the host member the package lacks links the host's file"
        );
    }

    #[test]
    fn a_damaged_view_is_rebuilt_for_the_next_launch() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        let counterpart = carry_counterpart(&w);
        let module = counterpart.join("libplug.so");
        fs::write(&module, module_bytes(0, &["libdep.so.2"], &[])).unwrap();
        let bundled = w.pkg.join("lib/alsa-lib");
        fs::create_dir_all(&bundled).unwrap();
        fs::write(bundled.join("libcarried.so"), b"bundled").unwrap();
        fs::create_dir_all(&w.store).unwrap();

        let r = w.resolve(HostLibsPolicy::Auto);
        let view = r.plugin_views[0].dir.clone();
        let planted = w.store.join("planted.so");
        fs::write(&planted, b"planted").unwrap();
        let link = view.join("libplug.so");
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&planted, &link).unwrap();
        std::os::unix::fs::symlink(&planted, view.join("extra.so")).unwrap();

        let r = w.resolve(HostLibsPolicy::Auto);
        assert_eq!(r.plugin_views.len(), 1);
        let view = r.plugin_views[0].dir.clone();
        let mut names: Vec<String> = fs::read_dir(&view)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["libcarried.so", "libplug.so"]);
        assert_eq!(
            fs::read_link(view.join("libplug.so")).ok(),
            Some(module),
            "the bent link is re-aimed at the host's module"
        );
        assert_eq!(
            fs::read_link(view.join("libcarried.so")).ok(),
            Some(bundled.join("libcarried.so"))
        );
    }

    #[test]
    fn a_served_record_also_presents_its_views() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        let counterpart = carry_counterpart(&w);
        let module = counterpart.join("libplug.so");
        fs::write(&module, module_bytes(0, &["libdep.so.2"], &[])).unwrap();
        let bundled = w.pkg.join("lib/alsa-lib");
        fs::create_dir_all(&bundled).unwrap();
        fs::write(bundled.join("libcarried.so"), b"bundled").unwrap();
        fs::create_dir_all(&w.store).unwrap();

        let first = w.resolve(HostLibsPolicy::Auto);
        let record = sel_dir(&w.store, &[]).join("decision");
        let before = fs::metadata(&record).unwrap().modified().unwrap();

        let second = w.resolve(HostLibsPolicy::Auto);
        let after = fs::metadata(&record).unwrap().modified().unwrap();
        assert_eq!(before, after, "the second launch is served by the record");
        assert_eq!(second.plugin_views, first.plugin_views);
        assert_eq!(
            fs::read_link(second.plugin_views[0].dir.join("libplug.so")).ok(),
            Some(module)
        );
    }

    #[test]
    fn a_launch_without_admitted_modules_builds_no_view() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        carry_counterpart(&w);
        let bundled = w.pkg.join("lib/alsa-lib");
        fs::create_dir_all(&bundled).unwrap();
        fs::write(bundled.join("libcarried.so"), b"bundled").unwrap();
        fs::create_dir_all(&w.store).unwrap();

        let r = w.resolve(HostLibsPolicy::Auto);
        assert!(r.plugin_views.is_empty());
        let strangers: Vec<String> = fs::read_dir(&w.store)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("view-"))
            .collect();
        assert!(
            strangers.is_empty(),
            "no view root is built without admitted members"
        );
    }

    /// A file that is only an ELF header has nothing the loader can
    /// map, so admission refuses it instead of presenting a module that
    /// fails at launch.
    #[test]
    fn a_header_only_module_is_refused() {
        let w = World::new();
        w.bundle("libc.so.6", &versioned(&["GLIBC_2.32"]));
        w.host("libdep.so.2", &versioned(&["DEP_1.0"]));
        let dir = carry_counterpart(&w);
        let module = dir.join("libhead.so");
        let mut head = vec![0u8; 64];
        head[0..4].copy_from_slice(b"\x7fELF");
        head[4] = 2;
        head[5] = 1;
        head[16..18].copy_from_slice(&3u16.to_le_bytes());
        fs::write(&module, head).unwrap();

        let decision = w.decide(HostLibsPolicy::Auto);
        assert!(decision.admitted.is_empty());
        let refusal = refusal_for(&decision.refused, &module);
        assert!(refusal.reason.contains("map"), "{refusal:?}");
    }

    #[test]
    fn an_icd_naming_its_driver_by_bare_soname_seeds_the_walk() {
        let w = World::new();
        // NVIDIA's files name the vendor library by soname alone, and no
        // driver family prefix matches `libGLX_nvidia`.
        w.host(
            "libGLX_nvidia.so.0",
            &driver_needing(&["libnvidia-glcore.so.1"]),
        );
        let host_glcore = w.host("libnvidia-glcore.so.1", &versioned(&[]));
        fs::write(
            w.icd_dir.join("nvidia_icd.json"),
            r#"{ "ICD": { "library_path": "libGLX_nvidia.so.0" } }"#,
        )
        .unwrap();
        w.bundle("libapp.so.1", &versioned(&[]));

        w.resolve(HostLibsPolicy::Auto);
        assert!(w.farm_link("libGLX_nvidia.so.0").is_some());
        assert_eq!(w.farm_link("libnvidia-glcore.so.1"), Some(host_glcore));
    }

    #[test]
    fn a_glx_vendor_library_nothing_names_is_farmed_with_its_needs() {
        let w = World::new();
        // glvnd builds the vendor's soname from the X server's vendor
        // string, so no ICD or EGL vendor file names Mesa's copy and no
        // driver family prefix matches it.
        let host_mesa = w.host("libGLX_mesa.so.0", &driver_needing(&["libxcb-glx.so.0"]));
        let host_xcb_glx = w.host("libxcb-glx.so.0", &versioned(&[]));
        w.bundle("libapp.so.1", &versioned(&[]));

        w.resolve(HostLibsPolicy::Auto);
        assert_eq!(w.farm_link("libGLX_mesa.so.0"), Some(host_mesa));
        assert_eq!(w.farm_link("libxcb-glx.so.0"), Some(host_xcb_glx));
    }

    #[test]
    fn the_glx_indirect_fallback_is_farmed() {
        let w = World::new();
        let host_indirect = w.host("libGLX_indirect.so.0", &versioned(&[]));
        w.bundle("libapp.so.1", &versioned(&[]));

        w.resolve(HostLibsPolicy::Auto);
        assert_eq!(w.farm_link("libGLX_indirect.so.0"), Some(host_indirect));
    }

    #[test]
    fn a_bundled_glx_vendor_stays_bundled() {
        let w = World::new();
        // Neither copy defines symbol versions, so the sets are equal and
        // the package ships its own driver choice.
        w.host("libGLX_mesa.so.0", &versioned(&[]));
        w.bundle("libGLX_mesa.so.0", &versioned(&[]));

        let icd_dir = w.icd_dir.to_string_lossy().into_owned();
        let req = Request {
            pkg_root: &w.pkg,
            lib_dirs: &["lib"],
            plugin_dirs: &[],
            policy: HostLibsPolicy::Auto,
            store: &w.store,
            ld_cache: &w.cache,
            icd_dirs: &[icd_dir.as_str()],
            extra_root: None,
            set_roots: &[],
            host_libc: true,
            loader_store: w.loader_store(),
            nsswitch: &w.nsswitch,
        };
        let d = decide(&req);
        assert_eq!(
            d.compared,
            vec![("libGLX_mesa.so.0".to_string(), Some(Choice::Bundle))]
        );
        assert!(d.winners.is_empty());

        w.resolve(HostLibsPolicy::Auto);
        assert!(w.farm_link("libGLX_mesa.so.0").is_none());
    }

    #[test]
    fn a_host_without_glx_vendors_is_unaffected() {
        let w = World::new();
        let host_gl = w.host("libGL.so.1", &driver_needing(&["libfoo.so.1"]));
        w.host("libfoo.so.1", &versioned(&["FOO_1.0", "FOO_1.1"]));
        w.bundle("libfoo.so.1", &versioned(&["FOO_1.0"]));

        let closure = HostIndex::load(&w.cache, &[], None).driver_closure();
        assert!(!closure.iter().any(|n| n.starts_with("libGLX_")));

        w.resolve(HostLibsPolicy::Auto);
        assert_eq!(w.farm_link("libGL.so.1"), Some(host_gl));
        assert!(w.farm_link("libfoo.so.1").is_some());
    }

    #[test]
    fn a_dri_module_brings_what_it_needs() {
        let w = World::new();
        let host_llvm = w.host("libLLVM-15.so.1", &versioned(&[]));
        w.host_module("swrast_dri.so", &driver_needing(&["libLLVM-15.so.1"]));
        w.bundle("libapp.so.1", &versioned(&[]));

        w.resolve(HostLibsPolicy::Auto);
        assert_eq!(w.farm_link("libLLVM-15.so.1"), Some(host_llvm));
        assert!(w.farm_link("swrast_dri.so").is_none());
    }

    #[test]
    fn an_unreadable_module_is_ignored() {
        let w = World::new();
        let host_llvm = w.host("libLLVM-15.so.1", &versioned(&[]));
        w.host_module("broken_dri.so", b"not an ELF object");
        w.host_module("swrast_dri.so", &driver_needing(&["libLLVM-15.so.1"]));
        w.bundle("libapp.so.1", &versioned(&[]));

        w.resolve(HostLibsPolicy::Auto);
        assert_eq!(w.farm_link("libLLVM-15.so.1"), Some(host_llvm));
    }

    #[test]
    fn a_new_module_resolves_afresh() {
        let w = World::new();
        w.host("libLLVM-15.so.1", &versioned(&[]));
        let host_sensors = w.host("libsensors.so.5", &versioned(&[]));
        w.host_module("swrast_dri.so", &driver_needing(&["libLLVM-15.so.1"]));
        w.bundle("libapp.so.1", &versioned(&[]));
        w.resolve(HostLibsPolicy::Auto);
        assert!(w.farm_link("libsensors.so.5").is_none());

        w.host_module("radeonsi_dri.so", &driver_needing(&["libsensors.so.5"]));
        // Set explicitly: a directory's mtime can be coarser than the
        // time between the two writes.
        fs::File::open(w.host_lib.join("dri"))
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
            .unwrap();
        w.resolve(HostLibsPolicy::Auto);
        assert_eq!(w.farm_link("libsensors.so.5"), Some(host_sensors));
    }

    #[test]
    fn equivalent_selection_lists_share_one_identity() {
        let (gtk, qt) = (pin("gtk", 1), pin("qt", 2));
        let one = SelectionIdentity::new(&[gtk.clone(), qt.clone()]).unwrap();
        let two = SelectionIdentity::new(&[qt.clone(), gtk.clone()]).unwrap();
        assert_eq!(one, two);
        assert_eq!(one.canonical(), two.canonical());
        assert_eq!(one.key(), two.key());

        // A hash written in the other case names the same build.
        let mut upper = pin("qt", 2);
        upper.hash = upper.hash.to_ascii_uppercase();
        assert_eq!(one, SelectionIdentity::new(&[gtk, upper]).unwrap());
    }

    #[test]
    fn a_changed_subset_or_pin_changes_the_identity() {
        let gtk = pin("gtk", 1);
        let qt = pin("qt", 2);
        let gtk_repinned = pin("gtk", 3);
        let gtk_renamed = pin("toolkit", 1);

        let full = SelectionIdentity::new(&[gtk.clone(), qt.clone()]).unwrap();
        let subset = SelectionIdentity::new(std::slice::from_ref(&gtk)).unwrap();
        let empty = SelectionIdentity::new(&[]).unwrap();
        let repinned = SelectionIdentity::new(&[gtk_repinned]).unwrap();
        assert_ne!(full, subset, "a dropped set is another selection");
        assert_ne!(subset, empty, "an empty selection is its own");
        assert_ne!(subset, repinned, "a republished pin is another selection");
        assert_ne!(
            subset,
            SelectionIdentity::new(&[gtk_renamed]).unwrap(),
            "a renamed declaration is another selection"
        );
        assert_ne!(full.key(), subset.key());
        assert_ne!(subset.key(), empty.key());

        // The key is a bounded lowercase hex name.
        let key = full.key();
        assert_eq!(key.len(), 32);
        assert!(key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
    }

    #[test]
    fn duplicate_pins_collapse_and_conflicting_ones_do_not() {
        let (gtk_a, gtk_b) = (pin("gtk", 1), pin("gtk", 2));
        let twice = SelectionIdentity::new(&[gtk_a.clone(), gtk_a.clone()]).unwrap();
        assert_eq!(
            twice,
            SelectionIdentity::new(std::slice::from_ref(&gtk_a)).unwrap()
        );

        // The same name pinned two ways stays both pins, in a stable
        // order, and never shares an identity with either alone.
        let both = SelectionIdentity::new(&[gtk_a.clone(), gtk_b.clone()]).unwrap();
        assert_ne!(both, twice);
        assert_eq!(
            both,
            SelectionIdentity::new(&[gtk_b.clone(), gtk_a.clone()]).unwrap()
        );
        assert_eq!(both.pins(), &[gtk_a, gtk_b]);
    }

    #[test]
    fn a_pin_outside_the_bounds_refuses_an_identity() {
        let ok = pin("gtk", 1);
        let refused = |pins: &[ActivePin]| SelectionIdentity::new(pins).is_none();
        let with_label = |label: &str| ActivePin {
            label: label.to_string(),
            hash: ok.hash.clone(),
        };
        let with_hash = |hash: String| ActivePin {
            label: "gtk".to_string(),
            hash,
        };

        assert!(refused(&[ActivePin {
            label: String::new(),
            hash: ok.hash.clone()
        }]));
        assert!(refused(&[with_label(&"g".repeat(129))]));
        for label in ["a\tb", "a\nb", "a\rb", "a\u{1}b", "a\u{7f}b", "a\u{0}b"] {
            assert!(refused(&[with_label(label)]), "name {label:?} is refused");
        }
        for hash in [
            String::new(),
            "ab".repeat(31),
            "ab".repeat(33),
            "zz".repeat(32),
        ] {
            assert!(refused(&[with_hash(hash)]), "hash is refused");
        }

        // Exactly the pin-count bound is fine, one past it is not.
        let at_bound: Vec<ActivePin> = (0..MAX_ACTIVE_PINS as u32)
            .map(|i| pin(&format!("s{i}"), 1))
            .collect();
        assert!(SelectionIdentity::new(&at_bound).is_some());
        let mut past_bound = at_bound.clone();
        past_bound.push(pin("extra", 1));
        assert!(refused(&past_bound));
    }

    #[test]
    fn a_selection_directory_refuses_what_it_did_not_mint() {
        assert_eq!(selection_dir(""), None);
        assert_eq!(selection_dir("../escape"), None);
        assert_eq!(selection_dir("a/b"), None);
        assert_eq!(selection_dir("ABC"), None);
        assert_eq!(selection_dir("gg"), None);
        assert_eq!(selection_dir(&"a".repeat(65)), None);
        assert_eq!(
            selection_dir(&"a".repeat(32)),
            Some(format!("sel-{}", "a".repeat(32)))
        );
    }

    /// Two launches naming the same active declarations in opposite list
    /// orders share one record: the second is served from it rather than
    /// recomputing, seen in the record going untouched while the farm it
    /// names still serves both.
    #[test]
    fn selection_list_order_does_not_invalidate_the_record() {
        let w = World::new();
        w.host("libGL.so.1", &driver_needing(&["libfoo.so.1"]));
        let host_foo = w.host("libfoo.so.1", &versioned(&["FOO_1.0", "FOO_1.1"]));
        w.bundle("libfoo.so.1", &versioned(&["FOO_1.0"]));
        let (gtk, qt) = (pin("gtk", 1), pin("qt", 2));

        let first = w.resolve_selection(HostLibsPolicy::Auto, &[], &[gtk.clone(), qt.clone()]);
        assert_eq!(w.farm_link("libfoo.so.1"), Some(host_foo.clone()));
        let record = sel_dir(&w.store, &[gtk.clone(), qt.clone()]).join(DECISION_FILE);
        assert!(record.is_file());

        let stamped = backdate(&record);
        let second = w.resolve_selection(HostLibsPolicy::Auto, &[], &[qt, gtk]);
        assert_eq!(second, first);
        assert_eq!(
            fs::metadata(&record).unwrap().modified().unwrap(),
            stamped,
            "an equivalent launch reuses the record instead of rewriting it"
        );
        assert_eq!(w.farm_link("libfoo.so.1"), Some(host_foo));
    }

    /// A changed subset or a republished pin is another library universe:
    /// its record is separate, and the launch decides against only its
    /// own roots instead of being served the other selection's decision.
    #[test]
    fn a_changed_subset_or_pin_recomputes_in_its_own_universe() {
        let w = World::new();
        w.host(
            "libGL.so.1",
            &driver_needing(&["libfoo.so.1", "libbar.so.1"]),
        );
        let host_foo = w.host("libfoo.so.1", &versioned(&["FOO_1.0", "FOO_1.1"]));
        let host_bar = w.host("libbar.so.1", &versioned(&["BAR_1.0"]));
        w.bundle("libapp.so.1", &versioned(&[]));

        let root = temp_dir("sel-universe");
        let one = set_lib(&root.join("one"), "libfoo.so.1", &versioned(&["FOO_1.0"]));
        let two = set_lib(
            &root.join("two"),
            "libbar.so.1",
            &versioned(&["BAR_1.0", "BAR_2.0"]),
        );
        let (gtk, qt) = (pin("gtk", 1), pin("qt", 2));

        w.resolve_selection(
            HostLibsPolicy::Auto,
            &[one.clone(), two],
            &[gtk.clone(), qt.clone()],
        );
        assert_eq!(w.farm_link("libfoo.so.1"), Some(host_foo.clone()));
        assert!(
            w.farm_link("libbar.so.1").is_none(),
            "the set's newer bar stays bundled"
        );
        let toolkit_record = sel_dir(&w.store, &[gtk.clone(), qt.clone()]).join(DECISION_FILE);
        assert!(toolkit_record.is_file());

        // Dropping the set whose library won is another selection: the
        // host's older bar is now the only bar, and the toolkit's
        // decision must not stand in for it.
        let stamped = backdate(&toolkit_record);
        w.resolve_selection(HostLibsPolicy::Auto, &[one], std::slice::from_ref(&gtk));
        assert_eq!(w.farm_link("libbar.so.1"), Some(host_bar.clone()));
        assert_eq!(w.farm_link("libfoo.so.1"), Some(host_foo.clone()));
        let gtk_record = sel_dir(&w.store, std::slice::from_ref(&gtk)).join(DECISION_FILE);
        assert!(gtk_record.is_file());
        assert_ne!(
            gtk_record.parent(),
            toolkit_record.parent(),
            "another selection records beside, not over"
        );
        assert_eq!(
            fs::metadata(&toolkit_record).unwrap().modified().unwrap(),
            stamped,
            "the other selection's record is kept, not edited"
        );

        // The same declaration republished under a new pin is another
        // selection again: this build's foo matches the host's, so it
        // stays bundled where the older pin's foo was farmed.
        let gtk2 = pin("gtk", 3);
        let one2 = set_lib(
            &root.join("one-2"),
            "libfoo.so.1",
            &versioned(&["FOO_1.0", "FOO_1.1"]),
        );
        w.resolve_selection(HostLibsPolicy::Auto, &[one2], std::slice::from_ref(&gtk2));
        assert!(
            w.farm_link("libfoo.so.1").is_none(),
            "the repinned set's foo stays bundled"
        );
        assert!(sel_dir(&w.store, &[gtk2]).join(DECISION_FILE).is_file());
        let _ = fs::remove_dir_all(&root);
    }

    /// A launch with no sets active shares the host with a toolkit launch
    /// but not its universe: the host's own bar is the only bar, where
    /// the toolkit's decision kept the set's newer one.
    #[test]
    fn an_empty_selection_is_its_own_universe() {
        let w = World::new();
        w.host(
            "libGL.so.1",
            &driver_needing(&["libfoo.so.1", "libbar.so.1"]),
        );
        let host_bar = w.host("libbar.so.1", &versioned(&["BAR_1.0"]));
        w.host("libfoo.so.1", &versioned(&["FOO_1.0", "FOO_1.1"]));
        w.bundle("libapp.so.1", &versioned(&[]));

        let root = temp_dir("sel-empty");
        let two = set_lib(
            &root.join("two"),
            "libbar.so.1",
            &versioned(&["BAR_1.0", "BAR_2.0"]),
        );

        let toolkit = w.resolve_selection(HostLibsPolicy::Auto, &[two], &[pin("qt", 2)]);
        assert!(toolkit.farm.is_some());
        assert!(w.farm_link("libbar.so.1").is_none());

        let bare = w.resolve_selection(HostLibsPolicy::Auto, &[], &[]);
        assert!(bare.farm.is_some());
        assert_eq!(w.farm_link("libbar.so.1"), Some(host_bar));
        assert!(sel_dir(&w.store, &[]).join(DECISION_FILE).is_file());
        let _ = fs::remove_dir_all(&root);
    }

    /// The record names its identity in full, so a reader pointed at one
    /// selection's directory with another selection's identity, or with
    /// another host state, still refuses it.
    #[test]
    fn a_selection_record_is_bound_to_its_identity_and_host() {
        let w = World::new();
        w.host("libGL.so.1", &driver_needing(&["libfoo.so.1"]));
        let host_foo = w.host("libfoo.so.1", &versioned(&["FOO_1.0", "FOO_1.1"]));
        w.bundle("libfoo.so.1", &versioned(&["FOO_1.0"]));
        let pins = [pin("gtk", 1)];
        let identity = SelectionIdentity::new(&pins).unwrap();
        let icd_dir = w.icd_dir.to_string_lossy().into_owned();
        let set_roots: Vec<PathBuf> = Vec::new();
        let fingerprint = fingerprint(
            &w.cache,
            &[],
            &[icd_dir.as_str()],
            HostLibsPolicy::Auto,
            None,
            true,
            &w.nsswitch,
            w.loader_store(),
        );

        let decided = w.resolve_selection(HostLibsPolicy::Auto, &set_roots, &pins);
        assert_eq!(w.farm_link("libfoo.so.1"), Some(host_foo.clone()));
        let dir = sel_dir(&w.store, &pins);

        let again =
            load_selection(&dir, &identity, &fingerprint, &w.store, w.loader_store()).unwrap();
        assert_eq!(again.farm.as_ref(), decided.farm.as_ref());

        let other = SelectionIdentity::new(&[pin("qt", 2)]).unwrap();
        assert_eq!(
            load_selection(&dir, &other, &fingerprint, &w.store, w.loader_store()),
            None
        );
        assert_eq!(
            load_selection(&dir, &identity, "another host", &w.store, w.loader_store()),
            None
        );
    }

    /// Hostile or damaged state is a miss with nothing adopted: another
    /// version's header, truncation, bytes that are not a record, a
    /// tampered identity line and an oversized file all recompute.
    #[test]
    fn hostile_selection_records_are_misses() {
        let w = World::new();
        w.host("libGL.so.1", &driver_needing(&["libfoo.so.1"]));
        w.host("libfoo.so.1", &versioned(&["FOO_1.0", "FOO_1.1"]));
        w.bundle("libfoo.so.1", &versioned(&["FOO_1.0"]));
        let pins = [pin("gtk", 1)];
        let identity = SelectionIdentity::new(&pins).unwrap();
        let icd_dir = w.icd_dir.to_string_lossy().into_owned();
        let set_roots: Vec<PathBuf> = Vec::new();
        let fingerprint = fingerprint(
            &w.cache,
            &[],
            &[icd_dir.as_str()],
            HostLibsPolicy::Auto,
            None,
            true,
            &w.nsswitch,
            w.loader_store(),
        );
        w.resolve_selection(HostLibsPolicy::Auto, &set_roots, &pins);
        let dir = sel_dir(&w.store, &pins);
        let record = dir.join(DECISION_FILE);
        let good = fs::read_to_string(&record).unwrap();
        let miss = |bytes: &[u8]| {
            fs::write(&record, bytes).unwrap();
            assert_eq!(
                load_selection(&dir, &identity, &fingerprint, &w.store, w.loader_store()),
                None
            );
        };

        miss(b"");
        miss(b"\xff\xfe not utf-8");
        miss(b"onelf-resolve 2\nfingerprint x\n");
        miss(b"onelf-resolve-selection 2\nfingerprint x\n");
        miss(b"onelf-resolve-selection 1\n");
        miss(&good.as_bytes()[..good.len() / 2]);
        miss(
            good.replacen(identity.canonical(), "pin 3:gtk\tzz", 1)
                .as_bytes(),
        );
        let mut junk = good.clone();
        junk.push_str("not-a-record-line\n");
        miss(junk.as_bytes());
        let oversized = vec![b'x'; (RECORD_MAX_BYTES + 1) as usize];
        miss(&oversized);

        fs::write(&record, &good).unwrap();
        assert!(
            load_selection(&dir, &identity, &fingerprint, &w.store, w.loader_store()).is_some(),
            "the intact record is still a hit"
        );
    }

    /// The record replaces any earlier one through a temp file and a
    /// rename, so a directory never holds a half-written decision and no
    /// scratch file outlives the write.
    #[test]
    fn recording_replaces_the_record_atomically() {
        let w = World::new();
        w.host("libGL.so.1", &driver_needing(&["libfoo.so.1"]));
        w.host("libfoo.so.1", &versioned(&["FOO_1.0", "FOO_1.1"]));
        w.bundle("libfoo.so.1", &versioned(&["FOO_1.0"]));
        let pins = [pin("gtk", 1)];
        let names = |dir: &Path| {
            let mut out: Vec<String> = fs::read_dir(dir)
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            out.sort();
            out
        };

        w.resolve_selection(HostLibsPolicy::Auto, &[], &pins);
        let dir = sel_dir(&w.store, &pins);
        assert_eq!(names(&dir), ["decision"]);

        // A record that fails its checks is rewritten in place, and the
        // rewrite again leaves only the decision behind.
        fs::write(dir.join(DECISION_FILE), b"").unwrap();
        w.resolve_selection(HostLibsPolicy::Auto, &[], &pins);
        assert_eq!(names(&dir), ["decision"]);
    }

    /// A record a release keyed on the host alone left at the store's top
    /// is never read: its winner is not served, and the launch decides
    /// for its own selection.
    #[test]
    fn a_host_only_record_is_never_served() {
        let w = World::new();
        w.host("libGL.so.1", &driver_needing(&["libfoo.so.1"]));
        let host_foo = w.host("libfoo.so.1", &versioned(&["FOO_1.0"]));
        w.bundle("libfoo.so.1", &versioned(&["FOO_1.0", "FOO_1.1"]));
        fs::create_dir_all(&w.store).unwrap();
        fs::write(
            w.store.join(DECISION_FILE),
            format!(
                "onelf-resolve 2\nfingerprint x\nhost libfoo.so.1\t{}\n",
                host_foo.display()
            ),
        )
        .unwrap();

        w.resolve_selection(HostLibsPolicy::Auto, &[], &[pin("gtk", 1)]);
        assert!(w.farm_link("libfoo.so.1").is_none());
        assert!(w.farm_link("libGL.so.1").is_some());
    }
}

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
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::drivers::{self, DRIVER_FAMILIES};
use crate::footer::HostLibsPolicy;
use crate::ldso::{self, MAX_LOADER_BYTES};
use crate::verdef::{self, Choice, VersionSet};

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

/// Whether `soname` belongs to the glibc family, which a package always
/// carries as a unit with its loader.
pub fn is_libc_family(soname: &str) -> bool {
    LIBC_FAMILY.iter().any(|p| soname.starts_with(p))
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
        req.icd_dirs,
        req.policy,
        req.extra_root,
        req.host_libc,
        req.nsswitch,
        req.loader_store,
    );
    let Some(identity) = SelectionIdentity::new(pins) else {
        return fresh(req, &fingerprint, &host).1;
    };
    let Some(dir) = selection_dir(identity.key()).map(|name| req.store.join(name)) else {
        return fresh(req, &fingerprint, &host).1;
    };
    if let Some(recorded) =
        load_selection(&dir, &identity, &fingerprint, req.store, req.loader_store)
    {
        return recorded;
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
    resolution
}

/// A fresh decision for `req` with its farm and loader copy built, and
/// whether it may be recorded.
///
/// When the closed loader copy cannot be kept after all, a race or a full
/// disk past what the runtime could judge up front, the request is
/// decided again with the store refused. The bundled pair runs and the
/// rest of the decision stands, the driver closure with it. That fallback
/// is not recorded, so a later launch tries the copy again.
fn fresh(req: &Request, fingerprint: &str, host: &HostIndex) -> (Decision, Resolution, bool) {
    let decision = decide_on(req, host);
    let Some(loader) = &decision.host_interp else {
        let resolution = materialized(req, fingerprint, &decision);
        return (decision, resolution, true);
    };
    match keep_loader_copy(loader, req.loader_store) {
        Ok(()) => {
            let resolution = materialized(req, fingerprint, &decision);
            (decision, resolution, true)
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
            let resolution = materialized(&refused, fingerprint, &decision);
            (decision, resolution, false)
        }
    }
}

/// The resolution of a decision, with its farm built. A farm that cannot
/// be written leaves nothing taken from the host.
fn materialized(req: &Request, fingerprint: &str, decision: &Decision) -> Resolution {
    let copy = decision.host_interp.as_ref().map(|l| l.copy.as_path());
    let farm = match materialize(req.store, fingerprint, &decision.winners, copy) {
        Ok(farm) => farm,
        Err(e) => {
            eprintln!(
                "onelf-rt: resolver: cannot write {}: {e}",
                req.store.display()
            );
            return Resolution::default();
        }
    };
    Resolution {
        farm,
        host_interp: decision.host_interp.as_ref().map(|loader| HostLoader {
            path: loader.copy.clone(),
            hash: loader.hash,
        }),
        incomparable: decision.incomparable.clone(),
    }
}

/// What the resolver decides for a request, worked out afresh and with
/// nothing written.
#[derive(Debug, Default)]
pub struct Decision {
    /// Every soname weighed, in order, with the copy that won: `None` for
    /// one the bundle lacks, which only the host can supply.
    pub compared: Vec<(String, Option<Choice>)>,
    /// The host libraries chosen, by soname.
    pub winners: BTreeMap<String, PathBuf>,
    /// The host loader the pair runs under, when the host's libc won and
    /// its loader could be closed.
    pub host_interp: Option<ClosedLoader>,
    /// The host loader of a winning host libc that could not be closed,
    /// and why. The bundled pair runs instead.
    pub host_interp_refused: Option<(PathBuf, String)>,
    pub incomparable: Vec<String>,
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
                                let path = if is_loader_soname(name) {
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

    // The driver closure is what the host is expected to supply. A member
    // the bundle also carries is compared; one it does not carry can only
    // come from the host. Under `always` every bundled soname is compared
    // as well.
    let mut candidates: Vec<String> = host.driver_closure().into_iter().collect();
    if decision.host_interp.is_some() {
        candidates.extend(host.closure(host_libc_opens(req.nsswitch)));
    }
    if req.policy == HostLibsPolicy::Always {
        candidates.extend(bundled.keys().cloned());
    }
    candidates.sort();
    candidates.dedup();
    for name in candidates {
        if is_libc_family(&name) {
            // A closed host loader finds nothing on its own, so a member
            // of its family that the host libc or a host driver opens and
            // the bundle lacks has to be chosen like any other library.
            if decision.host_interp.is_some()
                && !is_loader_soname(&name)
                && !bundled.contains_key(&name)
                && let Some(host_path) = host.libs.get(&name)
            {
                decision.compared.push((name.clone(), None));
                decision.winners.insert(name, host_path.clone());
            }
            continue;
        }
        let Some(host_path) = host.libs.get(&name) else {
            continue;
        };
        let choice = bundled
            .get(&name)
            .map(|bundled_path| verdef::compare(&versions(bundled_path), &versions(host_path)));
        decision.compared.push((name.clone(), choice));
        match choice.unwrap_or(Choice::Host) {
            Choice::Host => {
                decision.winners.insert(name, host_path.clone());
            }
            Choice::Incomparable => decision.incomparable.push(name),
            Choice::Bundle => {}
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
            admitted(pin)?;
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
fn admitted(pin: &ActivePin) -> Option<()> {
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
/// cache image, the name service configuration, the directories the cache
/// names and the driver description files, each with its size and
/// modification time, and where loader copies can be kept. Compared
/// whole, so no hash is needed.
fn fingerprint(
    ld_cache: &Path,
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
                out.push_str(&format!("={}:{mtime}", md.len()));
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
const SELECTION_RECORD_HEADER: &str = "onelf-resolve-selection 2";

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

fn is_loader_soname(name: &str) -> bool {
    name.starts_with("ld-linux")
}

/// Where the farm link for winner `name` points: the closed copy for the
/// loader when one runs, the chosen host file for everything else. The
/// copy is what a bundled loader's path shows once it is shadowed, so a
/// process that re-execs through that path gets the closed loader too.
fn link_target<'p>(name: &str, host: &'p Path, copy: Option<&'p Path>) -> &'p Path {
    match copy {
        Some(copy) if is_loader_soname(name) => copy,
        _ => host,
    }
}

/// A loader image read whole, refused when it is not a regular file or
/// is larger than any loader.
fn read_loader(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let file = fs::File::open(path)?;
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
    if farm.is_dir() {
        return Ok(Some(farm));
    }
    let scratch = store.join(format!(
        ".{}.{}",
        farm.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch)?;
    for (name, target) in winners {
        std::os::unix::fs::symlink(link_target(name, target, copy), scratch.join(name))?;
    }
    match fs::rename(&scratch, &farm) {
        Ok(()) => {}
        Err(_) if farm.is_dir() => {
            let _ = fs::remove_dir_all(&scratch);
        }
        Err(e) => {
            let _ = fs::remove_dir_all(&scratch);
            return Err(e);
        }
    }
    Ok(Some(farm))
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
    for name in &resolution.incomparable {
        text.push_str(&format!("incomparable {name}\n"));
    }
    let tmp = dir.join(format!(".{DECISION_FILE}.{}", std::process::id()));
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
/// from and the closed loader it runs, if any. Anything else is a miss;
/// nothing is adopted partially.
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
    for line in lines {
        if let Some(rest) = line.strip_prefix("interp ") {
            let (hash, name) = rest.split_once('\t')?;
            if loader.is_some() || !crate::manifest::is_safe_component(name) {
                return None;
            }
            loader = Some((blake3::Hash::from_hex(hash).ok()?, name.to_string()));
        } else if let Some(rest) = line.strip_prefix("host ") {
            let (name, path) = rest.split_once('\t')?;
            winners.insert(name.to_string(), PathBuf::from(path));
        } else if let Some(rest) = line.strip_prefix("stat ") {
            let (name, recorded) = rest.split_once('\t')?;
            let path = winners.get(name)?;
            if stat_line(path) != recorded {
                return None;
            }
        } else {
            let name = line.strip_prefix("incomparable ")?;
            resolution.incomparable.push(name.to_string());
        }
    }
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
    Some(farm)
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
        /// Where the host's ICD files go; empty unless a test writes one.
        icd_dir: PathBuf,
        /// The farm the last resolution produced, where `farm_link` looks.
        last_farm: std::cell::RefCell<Option<PathBuf>>,
        /// The cache root closed loader copies go under.
        loader_root: PathBuf,
        /// The host's name service configuration; absent unless a test
        /// writes one.
        nsswitch: PathBuf,
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
            HostLibsPolicy::Auto,
            None,
            true,
            &w.nsswitch,
            w.loader_store(),
        );
        assert_ne!(before, after);
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

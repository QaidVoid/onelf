//! Libraries from outside the package, shown inside its library directory.
//!
//! A launch can take libraries from two places besides the bundle: the
//! resolver's link farm, which holds the host libraries it chose and those
//! of a fetched GL build, and the dependency sets the package pins. Every
//! bundled executable carries a `$ORIGIN` run path naming the bundle's
//! library directory, and that run path is a `DT_RPATH`, which the loader
//! also searches for every library below the executable. A link placed in
//! that directory is therefore found the way a bundled library is, and no
//! `LD_LIBRARY_PATH` is needed. The variable would reach every process the
//! app spawns, and host programs would load the same libraries.
//!
//! Only names the package does not carry are added. A host library that
//! beat a bundled copy takes that copy's place by other means (see
//! `launch::shadow_bundled_copies`).
//!
//! A tree extracted for one launch, and the shared cache tree, get real
//! symlinks. A FUSE mount serves them as entries of its own, which the
//! server learns from the launching child through a [`channel`].

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use onelf_format::Manifest;
use onelf_format::entry::EntryKind;

/// Link name to target, ordered by name.
pub type Links = BTreeMap<String, PathBuf>;

/// The index of the directory entry at `rel`, relative to the package root.
pub(crate) fn dir_index(manifest: &Manifest, rel: &str) -> Option<usize> {
    (0..manifest.entries.len())
        .find(|&i| manifest.entries[i].kind == EntryKind::Dir && manifest.entry_path(i) == rel)
}

/// The directory the links go in, relative to the package root: the
/// shallowest one inside the package that the entrypoint's `$ORIGIN` run
/// path names, else the first library directory the package lists.
/// `runpath` holds the run path's directories as absolute paths.
///
/// The packer gives every object the entry that reaches the library
/// directory. A file in a subdirectory of it gets its own directory
/// first, and any directory its original run path named after, both
/// deeper than the library directory itself.
pub(crate) fn dir(
    manifest: &Manifest,
    pkg_root: &Path,
    runpath: &[PathBuf],
    lib_dirs: &[&str],
) -> Option<String> {
    dir_where(pkg_root, runpath, lib_dirs, |d| {
        !d.is_empty() && dir_index(manifest, d).is_some()
    })
}

/// [`dir`] for a tree without a manifest, such as a development view:
/// `exists` answers whether the package tree really holds the directory
/// a candidate names.
pub fn dir_where(
    pkg_root: &Path,
    runpath: &[PathBuf],
    lib_dirs: &[&str],
    exists: impl Fn(&str) -> bool,
) -> Option<String> {
    runpath
        .iter()
        .filter_map(|d| d.strip_prefix(pkg_root).ok()?.to_str())
        .filter(|d| exists(d))
        .min_by_key(|d| Path::new(d).components().count())
        .or_else(|| lib_dirs.iter().copied().find(|d| exists(d)))
        .map(String::from)
}

/// The names the package itself has in `dir`.
pub(crate) fn carried<'m>(manifest: &'m Manifest, dir: &str) -> HashSet<&'m str> {
    let Some(idx) = dir_index(manifest, dir) else {
        return HashSet::new();
    };
    manifest
        .entries
        .iter()
        .filter(|e| e.parent as usize == idx)
        .map(|e| manifest.get_string(e.name))
        .collect()
}

/// The links a launch needs in `dir`: every library in the farm and in the
/// sets that the package does not carry there, the farm first and the sets
/// in order, so the first source of a name keeps it.
///
/// A set is linked from every directory in it that holds a shared object,
/// subdirectories included. A set library loaded through a link takes the
/// link's directory as its `$ORIGIN`, so a dependency its run path finds
/// in a subdirectory of the set has to be reachable from here too.
pub(crate) fn wanted(
    manifest: &Manifest,
    dir: &str,
    farm: Option<&Path>,
    set_roots: &[PathBuf],
) -> Links {
    let carried: HashSet<String> = carried(manifest, dir)
        .into_iter()
        .map(String::from)
        .collect();
    wanted_from(farm, set_roots, &carried)
}

/// [`wanted`] for a tree without a manifest, such as a development view:
/// `carried` holds the names the tree itself already has in the target
/// directory, so nothing it carries is linked from outside.
pub fn wanted_from(farm: Option<&Path>, set_roots: &[PathBuf], carried: &HashSet<String>) -> Links {
    let mut links = Links::new();
    let mut add = |name: &OsStr, target: PathBuf| {
        if let Some(name) = name.to_str()
            && !carried.contains(name)
            && !links.contains_key(name)
        {
            links.insert(name.to_string(), target);
        }
    };
    if let Some(farm) = farm
        && let Ok(entries) = std::fs::read_dir(farm)
    {
        for entry in entries.flatten() {
            if let Ok(target) = std::fs::read_link(entry.path()) {
                add(&entry.file_name(), target);
            }
        }
    }
    for dir in set_roots.iter().flat_map(|r| crate::sets::lib_dirs(r)) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut found: Vec<_> = entries
            .flatten()
            .filter(|e| e.file_name().as_bytes().windows(3).any(|w| w == b".so"))
            .filter(|e| e.path().is_file())
            .map(|e| e.file_name())
            .collect();
        found.sort();
        for name in found {
            let target = dir.join(&name);
            add(&name, target);
        }
    }
    links
}

/// The links a launch before this one left in `dir` of the tree at `root`:
/// symlinks the package does not carry.
pub(crate) fn present(manifest: &Manifest, root: &Path, dir: &str) -> Links {
    let carried = carried(manifest, dir);
    present_excluding(root, dir, &carried)
}

/// [`present`] for a tree without a manifest: `carried` holds the names
/// the tree itself has in `dir`, which never count as placed links.
pub fn present_excluding(root: &Path, dir: &str, carried: &HashSet<&str>) -> Links {
    let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
        return Links::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_symlink()))
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            let target = std::fs::read_link(e.path()).ok()?;
            (!carried.contains(name.as_str())).then_some((name, target))
        })
        .collect()
}

/// Make `links` the links in `dir` of the tree at `root`, removing any a
/// previous launch left that this one does not need.
///
/// Each change is atomic on its own, so a launch sharing the tree never
/// finds a name half made. The cache tree is the shared one: launches on
/// the same host want the same links, and one made under another host
/// state replaces what differs.
///
/// A published view is refused outright: its links never change, and
/// anything that wants different ones stages a new view instead (see
/// [`view::ensure_view`]).
pub(crate) fn place(manifest: &Manifest, root: &Path, dir: &str, links: &Links) {
    if view::ready(root) {
        eprintln!(
            "onelf-rt: refusing to change the links of the published view {}: stage a new view instead",
            root.display()
        );
        return;
    }
    let carried = carried(manifest, dir);
    place_excluding(root, dir, links, &carried);
}

/// [`place`] for a tree without a manifest: `carried` holds the names
/// the tree itself has in `dir`, which are never removed or relinked.
pub(crate) fn place_excluding(root: &Path, dir: &str, links: &Links, carried: &HashSet<&str>) {
    let base = root.join(dir);
    let present = present_excluding(root, dir, carried);
    for name in present.keys().filter(|n| !links.contains_key(*n)) {
        if let Err(e) = std::fs::remove_file(base.join(name)) {
            eprintln!(
                "onelf-rt: cannot remove the stale link {}: {e}",
                base.join(name).display()
            );
        }
    }
    for (name, target) in links {
        if present.get(name) == Some(target) {
            continue;
        }
        let path = base.join(name);
        let scratch = base.join(format!(".{name}.{}.link", std::process::id()));
        let _ = std::fs::remove_file(&scratch);
        let made = std::os::unix::fs::symlink(target, &scratch)
            .and_then(|()| std::fs::rename(&scratch, &path));
        if let Err(e) = made {
            let _ = std::fs::remove_file(&scratch);
            eprintln!(
                "onelf-rt: cannot link {} to {}: {e}",
                path.display(),
                target.display()
            );
        }
    }
}

/// Whether a mount another launch made serves exactly `links` in `dir`,
/// and none in the package's other library directories.
pub fn served(
    manifest: &Manifest,
    root: &Path,
    dir: &str,
    links: &Links,
    lib_dirs: &[&str],
) -> bool {
    present(manifest, root, dir) == *links
        && lib_dirs
            .iter()
            .filter(|d| **d != dir)
            .all(|d| present(manifest, root, d).is_empty())
}

/// The directories an ELF's `$ORIGIN` run path names, resolved against
/// the file's own directory. Empty for a file that is not an ELF or names
/// no `$ORIGIN` in either `DT_RPATH` or `DT_RUNPATH`.
pub fn origin_runpath(target: &Path) -> Vec<PathBuf> {
    let Ok(data) = std::fs::read(target) else {
        return Vec::new();
    };
    let Ok(elf) = goblin::elf::Elf::parse(&data) else {
        return Vec::new();
    };
    let origin = target.parent().unwrap_or(Path::new("/"));
    elf.rpaths
        .iter()
        .chain(elf.runpaths.iter())
        .flat_map(|p| p.split(':'))
        .filter_map(|entry| {
            let rest = entry
                .strip_prefix("$ORIGIN")
                .or_else(|| entry.strip_prefix("${ORIGIN}"))?;
            Some(normalize(&origin.join(rest.trim_start_matches('/'))))
        })
        .collect()
}

/// `path` with `.` and `..` components folded away, without consulting
/// the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// The launching child's side of the hand-off to the FUSE server.
pub struct Sender {
    tx: OwnedFd,
    ack: OwnedFd,
}

/// The FUSE server's side of the hand-off.
pub(crate) struct Receiver {
    rx: OwnedFd,
    ack: OwnedFd,
    buf: Vec<u8>,
}

/// What the server has heard from the child so far.
pub(crate) enum Received {
    /// More is on its way.
    Pending,
    /// The child is done: its links, or nothing when it went away or sent
    /// something unreadable.
    Done(Option<Message>),
}

/// The links for one directory, the host libraries shown in place of
/// bundled copies, and the launch's generated environment input, as the
/// server is sent them.
pub(crate) struct Message {
    /// The directory, relative to the package root.
    pub dir: String,
    /// Each link's name and target.
    pub links: Vec<(String, Vec<u8>)>,
    /// Each shadowed file's path, relative to the package root, and the
    /// host file served in its place.
    pub shadows: Vec<(String, Vec<u8>)>,
    /// The generated environment input the mount serves; empty for none.
    pub env: Vec<u8>,
}

/// A pipe pair for handing the links from the child that resolves them to
/// the server that serves them. Made before the fork; the child keeps the
/// [`Sender`] and the server the [`Receiver`]. Every descriptor is
/// close-on-exec, so the app inherits none of them.
pub(crate) fn channel() -> io::Result<(Sender, Receiver)> {
    use rustix::pipe::{PipeFlags, pipe_with};
    let (rx, tx) = pipe_with(PipeFlags::CLOEXEC)?;
    let (ack_rx, ack_tx) = pipe_with(PipeFlags::CLOEXEC)?;
    Ok((
        Sender { tx, ack: ack_rx },
        Receiver {
            rx,
            ack: ack_tx,
            buf: Vec::new(),
        },
    ))
}

impl Sender {
    /// Send `links` for `dir`, the `shadows` to serve in place of bundled
    /// files (see [`Message::shadows`]) and the generated environment
    /// input `env`, and wait until the server serves them. A server that
    /// has gone away ends the wait as well.
    pub(crate) fn send(&self, dir: &str, links: &Links, shadows: &Links, env: &[u8]) {
        let mut body = (env.len() as u64).to_le_bytes().to_vec();
        body.extend_from_slice(env);
        body.extend_from_slice(dir.as_bytes());
        body.push(0);
        encode_pairs(&mut body, links);
        body.push(0);
        encode_pairs(&mut body, shadows);
        let mut message = (body.len() as u64).to_le_bytes().to_vec();
        message.extend_from_slice(&body);
        let mut rest = message.as_slice();
        while !rest.is_empty() {
            match rustix::io::write(&self.tx, rest) {
                Ok(n) => rest = &rest[n..],
                Err(rustix::io::Errno::INTR) => {}
                Err(_) => return,
            }
        }
        let mut byte = [0u8; 1];
        while let Err(rustix::io::Errno::INTR) = rustix::io::read(&self.ack, &mut byte) {}
    }
}

impl Receiver {
    pub(crate) fn fd(&self) -> &OwnedFd {
        &self.rx
    }

    /// Take what the pipe holds. Call when [`Receiver::fd`] polls readable.
    pub(crate) fn read(&mut self) -> Received {
        let mut chunk = [0u8; 64 * 1024];
        match rustix::io::read(&self.rx, &mut chunk) {
            Ok(0) => return Received::Done(None),
            Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
            Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => return Received::Pending,
            Err(_) => return Received::Done(None),
        }
        let Some(len) = self.buf.get(..8) else {
            return Received::Pending;
        };
        let len = u64::from_le_bytes(len.try_into().unwrap_or_default()) as usize;
        match self.buf.get(8..8usize.saturating_add(len)) {
            Some(body) => Received::Done(decode(body)),
            None => Received::Pending,
        }
    }

    /// Tell the child its links are served.
    pub(crate) fn ack(&self) {
        let _ = rustix::io::write(&self.ack, b"k");
    }
}

/// Append each name and target of `pairs`, every one NUL-terminated.
fn encode_pairs(body: &mut Vec<u8>, pairs: &Links) {
    for (name, target) in pairs {
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body.extend_from_slice(target.as_os_str().as_bytes());
        body.push(0);
    }
}

fn decode(body: &[u8]) -> Option<Message> {
    let env_len = usize::try_from(u64::from_le_bytes(body.get(..8)?.try_into().ok()?)).ok()?;
    let env = body.get(8..8usize.checked_add(env_len)?)?.to_vec();
    let body = &body[8 + env_len..];
    let mut fields = body.split(|&b| b == 0);
    let dir = std::str::from_utf8(fields.next()?).ok()?.to_string();
    let mut pairs = || -> Option<Vec<(String, Vec<u8>)>> {
        let mut pairs = Vec::new();
        while let Some(name) = fields.next().filter(|f| !f.is_empty()) {
            let target = fields.next()?;
            pairs.push((std::str::from_utf8(name).ok()?.to_string(), target.to_vec()));
        }
        Some(pairs)
    };
    let links = pairs()?;
    let shadows = pairs()?;
    Some(Message {
        dir,
        links,
        shadows,
        env,
    })
}

/// Execution-view identity and the configuration record stored beside a
/// view.
///
/// A launch-visible view is identified by everything that changes what
/// it shows or what the launch restores through it: the consuming uid,
/// the full package identity, the runtime metadata digest, the active
/// dependency-set pins, the effective policy over host libraries and the
/// libc and loader decision, how the library links reach the view, the
/// entrypoint where it shapes generated output, the effective links
/// themselves, and the generated per-view environment input with the
/// caller values and restoration records it carries.
///
/// The identity is a pure function of those values. It never consults
/// the filesystem it is about to create, and every occurrence of the
/// execution root in the environment input is replaced by a symbolic
/// placeholder first, so the key never depends recursively on the
/// absolute name of the directory it will name. Selections that differ
/// only in list order or hash case share one identity; a change to any
/// other input yields another.
///
/// Diagnostics name variables, never values. The `Debug` output of the
/// identity and of the record carries the key and scalar fields only,
/// and the canonical text, which does hold environment values, is never
/// rendered by either.
///
/// A view is built in private staging and becomes launch-visible in one
/// atomic step. [`ensure_view`] takes the view's creation mutex,
/// discards whatever an interrupted or hostile earlier attempt left,
/// lets the build fill a uniquely named staging tree beside the view,
/// and then publishes: the configuration record, the staged content and
/// its links are flushed, the ready record is written as the last
/// staging step, and one rename moves the complete tree onto the view's
/// name. A reader only ever resolves a view through its identity, so it
/// sees either nothing or a whole view with its ready record in place;
/// it never adopts staging, and a ready record that is missing, of
/// another layout, or damaged in any way is a miss that rebuilds.
///
/// Locks follow one canonical order: artifact claims and the content
/// store's lock first, then a view's in-use lease, then the view's
/// creation mutex. This module holds at most one view lock at a time,
/// the per-identity creation mutex, and never takes a store lock while
/// holding it: the caller pins its artifacts before calling, and a
/// build hardlinks store content as plain I/O without locking the
/// store. Reversing the order, a store lock held while a view lock is
/// awaited, is what a collector must never do; a build that keeps the
/// store's exclusive lock through a whole creation therefore cannot
/// deadlock against it.
///
/// Publication is final. No code path rewrites a published view: its
/// links are installed only in staging, `place` refuses a published
/// tree outright, and every content change the staging surface offers
/// goes through a fresh file renamed over the old name, so nothing ever
/// writes in place through a hardlink taken from the content store.
/// Changing a published view means staging and publishing a new one.
pub mod view {
    use std::ffi::OsStr;
    use std::fmt;
    use std::io::{self, Write};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    use onelf_format::HostLibsPolicy;
    use onelf_format::Manifest;
    use onelf_format::resolve::{ActivePin, SelectionIdentity};

    use super::Links;

    /// The view-layout version every identity names. A bump here
    /// separates every view an older runtime built from the ones the
    /// newer layout builds, and makes any record naming another version
    /// a miss rather than a misread.
    pub(crate) const LAYOUT: u32 = 1;

    /// The name of the configuration record inside a view directory.
    pub(crate) const CONFIGURATION_FILE: &str = "config";

    /// How many digest bytes a view directory's key keeps.
    const KEY_BYTES: usize = 16;
    /// The longest entrypoint name an identity admits.
    const MAX_ENTRY_NAME: usize = 128;
    /// The longest loader path an identity admits.
    const MAX_LOADER_BYTES: usize = 4096;
    /// The most links an identity admits.
    const MAX_VIEW_LINKS: usize = 256;
    /// The longest link name an identity admits: one path component.
    const MAX_LINK_NAME: usize = 255;
    /// The longest link target an identity admits.
    const MAX_LINK_TARGET: usize = 1024;
    /// The longest environment input an identity admits, the same bound
    /// the generated input is written under.
    const MAX_ENV_BYTES: usize = 1024 * 1024;
    /// The largest record a read accepts, above any admissible canonical
    /// text, so hostile state is a miss rather than a large read.
    const RECORD_MAX_BYTES: u64 = 4 * 1024 * 1024;
    /// What an occurrence of the execution root is replaced by in the
    /// environment input before the identity is computed.
    const ROOT_TOKEN: &[u8] = b"<view>";

    /// The inputs a view identity covers, as plain values. Paths appear
    /// only as values: computing the identity reads and writes nothing.
    pub struct ViewInputs<'a> {
        /// The user the view is built for.
        pub uid: u32,
        /// The package's full identity.
        pub package: &'a [u8; 32],
        /// The validated runtime metadata the launch applies, by its
        /// content digest; `None` for a package on the built-in
        /// defaults.
        pub metadata: Option<&'a [u8; 32]>,
        /// The dependency sets the launch activates, in any order.
        pub pins: &'a [ActivePin],
        /// The launch's policy over host libraries.
        pub policy: HostLibsPolicy,
        /// Whether the launch can put host files over bundled copies,
        /// which decides whether the host's libc can be taken at all.
        pub host_libc: bool,
        /// The loader the entrypoint runs under; `None` for the bundled
        /// one.
        pub loader: Option<&'a OsStr>,
        /// The entrypoint the view is generated for, where it shapes
        /// generated output.
        pub entrypoint: Option<&'a str>,
        /// The effective links the view shows, by name.
        pub links: &'a Links,
        /// The generated per-view environment input, with the caller
        /// values and restoration records it carries.
        pub runtime_env: &'a [u8],
        /// The execution root the view will live at. Used only to
        /// replace its occurrences in the environment input with a
        /// symbolic placeholder; never read, and never named by the
        /// identity.
        pub root: &'a OsStr,
    }

    /// The identity of one launch-visible view: the canonical text of
    /// its inputs and the bounded key that names its directory.
    ///
    /// Equal inputs give equal identities, including inputs that differ
    /// only in pin list order, pin hash case, or in which execution root
    /// the environment input names. The key digests the canonical text
    /// and only has to find a record; a record is compared against the
    /// full canonical text before it is trusted.
    #[derive(PartialEq, Eq)]
    pub struct ViewIdentity {
        uid: u32,
        canonical: Vec<u8>,
        key: String,
    }

    impl ViewIdentity {
        /// The identity of `inputs`, or `None` when an input lies
        /// outside the bounds a record can carry. A launch the identity
        /// cannot cover builds its view unnamed rather than under a
        /// guessed key.
        pub fn new(inputs: &ViewInputs<'_>) -> Option<Self> {
            let selection = SelectionIdentity::new(inputs.pins)?;
            admit(inputs.entrypoint.is_none_or(admitted_entry))?;
            admit(inputs.loader.is_none_or(admitted_loader))?;
            admit(inputs.links.len() <= MAX_VIEW_LINKS)?;
            for (name, target) in inputs.links {
                admit(admitted_link(name, target.as_os_str()))?;
            }
            let env = normalized_env(inputs.runtime_env, inputs.root)?;
            let canonical = canonical(inputs, &selection, &env);
            let digest = blake3::hash(&canonical);
            let key = crate::cache::hex(&digest.as_bytes()[..KEY_BYTES]);
            Some(Self {
                uid: inputs.uid,
                canonical,
                key,
            })
        }

        /// The bounded store name the identity is keyed by: lowercase
        /// hex over a digest of the canonical text. Equal identities
        /// share it.
        pub fn key(&self) -> &str {
            &self.key
        }

        /// The canonical text the identity is, as built. This is the
        /// exact text a configuration record stores and binds itself to.
        pub fn canonical(&self) -> &[u8] {
            &self.canonical
        }
    }

    /// The configuration record of one view, stored beside it.
    ///
    /// The record is the identity's canonical text followed by the state
    /// publication adds: the content hash of the environment input as
    /// finally expanded into the view. A reader can answer what
    /// configuration built the view from the record alone, without
    /// recomputing it, and refuses a record whose canonical text does
    /// not match the identity it was handed, so hostile or damaged
    /// storage is a miss rather than a misread.
    #[derive(PartialEq, Eq)]
    pub(crate) struct ViewRecord {
        canonical: Vec<u8>,
        expanded_env: Option<String>,
    }

    impl ViewRecord {
        /// Write the record for `identity` into `dir`, replacing any
        /// earlier one atomically. `expanded_env` is the environment
        /// input as finally expanded into the view, hashed here so no
        /// value is stored or echoed; `None` until the view is built.
        pub(crate) fn write(
            dir: &Path,
            identity: &ViewIdentity,
            expanded_env: Option<&[u8]>,
        ) -> io::Result<()> {
            std::fs::create_dir_all(dir)?;
            let mut out = identity.canonical().to_vec();
            match expanded_env {
                Some(bytes) => out.extend_from_slice(
                    format!(
                        "expanded {}\n",
                        crate::cache::hex(blake3::hash(bytes).as_bytes())
                    )
                    .as_bytes(),
                ),
                None => out.extend_from_slice(b"expanded -\n"),
            }
            let tmp = dir.join(format!(".{CONFIGURATION_FILE}.{}", std::process::id()));
            std::fs::File::create(&tmp)?.write_all(&out)?;
            std::fs::rename(&tmp, dir.join(CONFIGURATION_FILE))
        }

        /// The record beside `dir` that `identity` names, when it is
        /// intact, of this layout, and bound to exactly this identity.
        pub(crate) fn load(dir: &Path, identity: &ViewIdentity) -> Option<Self> {
            let path = dir.join(CONFIGURATION_FILE);
            let md = std::fs::metadata(&path).ok()?;
            if !md.is_file() || md.len() > RECORD_MAX_BYTES {
                return None;
            }
            let bytes = std::fs::read(&path).ok()?;
            let (canonical, expanded_env) = parse(&bytes)?;
            if canonical != identity.canonical() {
                return None;
            }
            Some(Self {
                canonical: canonical.to_vec(),
                expanded_env,
            })
        }

        /// Whether `dir` carries exactly the environment input the record
        /// hashes, or none when it records none. Anything else is damage.
        fn env_intact(&self, dir: &Path) -> bool {
            let path = dir.join(crate::env::VIEW_ENV_FILE);
            let Some(expected) = &self.expanded_env else {
                return std::fs::symlink_metadata(&path).is_err();
            };
            let Ok(md) = std::fs::symlink_metadata(&path) else {
                return false;
            };
            if !md.is_file() || md.len() > MAX_ENV_BYTES as u64 {
                return false;
            }
            std::fs::read(&path)
                .is_ok_and(|bytes| crate::cache::hex(blake3::hash(&bytes).as_bytes()) == *expected)
        }
    }

    /// The name of the ready record inside a view directory. The
    /// configuration record is [`ViewRecord`]'s; this one marks the
    /// whole view, record and content, as published.
    pub(crate) const READY_FILE: &str = "ready";

    /// The deepest relative path a staging write accepts, so a hostile
    /// name fails the build instead of nesting without bound.
    const MAX_REL_DEPTH: usize = 32;
    /// The longest relative path a staging write accepts.
    const MAX_REL_BYTES: usize = 4096;
    /// The separator that marks a staging tree as one identity's
    /// scratch: `view-<key>.staging-<pid>-<seq>` never matches the name
    /// [`dir_name`] mints, so no reader resolves a staging tree as a
    /// view. Collection classifies a store entry by it, the same way.
    pub(crate) const STAGING_SEP: &str = ".staging-";

    /// Whether `dir` holds a completely published view of this layout:
    /// the ready record must be exactly the bytes this layout writes,
    /// with nothing added, missing, or linked in from elsewhere.
    pub fn ready(dir: &Path) -> bool {
        let path = dir.join(READY_FILE);
        let Ok(md) = std::fs::symlink_metadata(&path) else {
            return false;
        };
        let bytes = ready_bytes();
        md.is_file()
            && md.len() == bytes.len() as u64
            && std::fs::read(&path).is_ok_and(|got| got == bytes)
    }

    /// The ready record this layout publishes: versioned like the
    /// configuration record, and adopted only byte for byte.
    fn ready_bytes() -> Vec<u8> {
        format!("onelf-view-ready {LAYOUT}\n").into_bytes()
    }

    /// The name of the staging tree a creator of `view` builds in,
    /// unique to this process and this attempt.
    fn staging_path(view: &Path) -> PathBuf {
        let seq = crate::cache::TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = view.file_name().and_then(|n| n.to_str()).unwrap_or("view");
        view.with_file_name(format!("{name}{STAGING_SEP}{}-{seq}", std::process::id()))
    }

    /// The name every staging tree of `view` starts with, so the next
    /// creator can reclaim the ones a dead creator left.
    fn staging_prefix(view: &Path) -> Option<String> {
        Some(format!("{}{STAGING_SEP}", view.file_name()?.to_str()?))
    }

    /// The creation mutex of `view`, a lock file kept beside it. The
    /// descriptor carries no inheritance: the mutex guards the build
    /// only, and dies at exec like any ordinary descriptor.
    pub(crate) fn creation_lock_path(view: &Path) -> Option<PathBuf> {
        Some(view.with_file_name(format!("{}.create", view.file_name()?.to_str()?)))
    }

    /// The in-use lease of `view`, a lock file kept beside it, held
    /// shared by a launch or its guardian for as long as the view backs
    /// a live process. A launch creates and locks the lease before it
    /// resolves or builds the view, and the descriptor is inheritable so
    /// the claim survives exec and descendant processes. Collection keys
    /// a view's idleness on this inode's flock alone, never on a pid
    /// recorded anywhere, so a detached helper that closed everything
    /// else still keeps its view.
    pub fn lease_lock_path(view: &Path) -> Option<PathBuf> {
        Some(view.with_file_name(format!("{}.lease", view.file_name()?.to_str()?)))
    }

    /// Take `view`'s creation mutex exclusively. Every creator of one
    /// identity serializes here, whatever process it runs in.
    fn creation_lock(view: &Path) -> io::Result<std::fs::File> {
        use rustix::fs::FlockOperation;
        let path = creation_lock_path(view).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "onelf: the view names no lock")
        })?;
        let lock = std::fs::File::create(&path)?;
        rustix::fs::flock(&lock, FlockOperation::LockExclusive)
            .map_err(|e| io::Error::other(format!("flock: {e}")))?;
        Ok(lock)
    }

    /// Build or reuse the view of `identity` under the private store
    /// `store`, and return it only once it is completely published.
    ///
    /// `lease` is the caller's in-use lease of this view (see
    /// [`lease_lock_path`]), already held shared. The canonical lock order
    /// is artifact claims and the store's lock first, then the lease, then
    /// the identity's creation mutex, which this function takes; it takes
    /// no store lock at all, so a creator holding the store's exclusive
    /// lock for its own publication cannot deadlock against it and the
    /// other way around.
    ///
    /// A ready view whose configuration record binds it to exactly this
    /// identity, and whose environment input is the one the record
    /// hashes, is reused untouched. Anything else is a miss. Every staging
    /// tree a dead creator left is discarded under the creation mutex, and
    /// `build` stages a fresh tree. Whatever is left at the view's name, a
    /// missing, foreign-versioned, or damaged ready record, a record of
    /// another identity, or an environment input that does not match, is
    /// replaced only when no other launch holds the lease: a damaged view
    /// that another launch still runs from is never removed under it, and
    /// the call fails instead.
    ///
    /// What the build returns is the environment input as finally
    /// expanded into the view. Publication writes it to
    /// [`crate::env::VIEW_ENV_FILE`] and the record carries its hash;
    /// `None` publishes a view that expands none. The build may leave
    /// nothing behind: its staging tree is removed whether it fails or
    /// succeeds.
    ///
    /// Every successful call stamps the lease's modification time, the
    /// last use collection measures a view's idle age by.
    pub fn ensure_view<F>(
        store: &Path,
        identity: &ViewIdentity,
        lease: &std::fs::File,
        build: F,
    ) -> io::Result<ViewReady>
    where
        F: FnOnce(&Staging) -> io::Result<Option<Vec<u8>>>,
    {
        let name = dir_name(identity.key()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "onelf: the view identity names no directory",
            )
        })?;
        let view = store.join(name);
        std::fs::create_dir_all(store)?;
        let _creation = creation_lock(&view)?;

        if ready(&view)
            && let Some(record) = ViewRecord::load(&view, identity)
            && record.env_intact(&view)
        {
            stamp_use(lease);
            return Ok(ViewReady { dir: view });
        }

        // A miss: staging may hold a dead creator's tree, which the
        // creation mutex proves nobody is filling, and the view's name
        // may hold a damaged or interrupted attempt.
        discard_staging(&view)?;
        if std::fs::symlink_metadata(&view).is_ok() {
            replace_unleased(&view, lease)?;
        }

        let staged = Staging::new(&view)?;
        let expanded = build(&staged)?;
        staged.publish(identity, expanded.as_deref())?;

        if ViewRecord::load(&view, identity).is_none() {
            return Err(io::Error::other(
                "onelf: the view record is unreadable right after publication",
            ));
        }
        stamp_use(lease);
        Ok(ViewReady { dir: view })
    }

    /// Remove what stands at `view`'s name, but only while `lease` is the
    /// only claim on it: the caller's shared lease is converted to an
    /// exclusive one without waiting, and back once the name is clear.
    ///
    /// A conversion that fails because another launch holds the lease
    /// drops the caller's shared lock, which is taken again before the
    /// error returns. Collection needs the creation mutex the caller
    /// holds, so nothing can reclaim the view in that window.
    fn replace_unleased(view: &Path, lease: &std::fs::File) -> io::Result<()> {
        use rustix::fs::FlockOperation;
        if rustix::fs::flock(lease, FlockOperation::NonBlockingLockExclusive).is_err() {
            rustix::fs::flock(lease, FlockOperation::LockShared)
                .map_err(|e| io::Error::other(format!("flock: {e}")))?;
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                "onelf: the execution view is damaged and another launch still runs from \
                 it; it is not replaced while in use",
            ));
        }
        let removed = remove_path(view);
        rustix::fs::flock(lease, FlockOperation::LockShared)
            .map_err(|e| io::Error::other(format!("flock: {e}")))?;
        removed
    }

    /// Record now as the view's last use, on the lease inode that outlives
    /// every sweep. The published view itself is never touched.
    fn stamp_use(lease: &std::fs::File) {
        let _ = lease.set_modified(std::time::SystemTime::now());
    }

    /// A view that was completely published and validated against its
    /// configuration record. Every access through this is read-only; a
    /// published view never changes.
    pub struct ViewReady {
        dir: PathBuf,
    }

    impl ViewReady {
        /// The view's directory on disk.
        pub fn dir(&self) -> &Path {
            &self.dir
        }
    }

    /// A private staging tree a creator fills before publication. The
    /// tree lives beside its view under the creator's own name, and is
    /// removed unless [`Staging::publish`] renames it onto the view.
    ///
    /// Every write goes through a name [`Staging::rel_path`]
    /// validated: relative, bounded, and free of traversal. Content is
    /// never written in place, so a file hardlinked from the content
    /// store keeps the store's bytes intact however much the staged
    /// copy is edited. The names [`ViewRecord::CONFIGURATION_FILE`]
    /// and [`READY_FILE`] at the tree's root belong to publication:
    /// whatever the build stages there is replaced when it publishes.
    pub struct Staging {
        dir: PathBuf,
        view: PathBuf,
    }

    impl Staging {
        /// Open a fresh staging tree for `view`, replacing any tree a
        /// recycled process id and sequence left at the same name.
        fn new(view: &Path) -> io::Result<Self> {
            let dir = staging_path(view);
            remove_path(&dir)?;
            std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
            Ok(Self {
                dir,
                view: view.to_path_buf(),
            })
        }

        /// The staging tree's root, for readers that verify what the
        /// build staged.
        pub fn dir(&self) -> &Path {
            &self.dir
        }

        /// `rel` resolved inside the staging tree, or an error for any
        /// path that is not a bounded relative path of ordinary
        /// components.
        fn rel_path(&self, rel: &str) -> io::Result<PathBuf> {
            admit_rel(rel)?;
            Ok(self.dir.join(rel))
        }

        /// Hardlink the regular file at `source`, normally a content
        /// store blob, into the staging tree at `rel`. The link shares
        /// the source's inode, so nothing may ever write it in place;
        /// content changes go through [`Staging::edit_file`].
        pub fn hardlink(&self, source: &Path, rel: &str) -> io::Result<()> {
            if !std::fs::symlink_metadata(source).is_ok_and(|md| md.is_file()) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "onelf: the content to link into the view is not a regular file",
                ));
            }
            let path = self.rel_path(rel)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::hard_link(source, &path)
        }

        /// Write `bytes` to `rel` in the staging tree with `mode`,
        /// replacing any file the build put there, through a fresh file
        /// renamed over the name.
        pub fn write_file(&self, rel: &str, bytes: &[u8], mode: u32) -> io::Result<()> {
            let path = self.rel_path(rel)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp = temp_name(&path);
            let write = || -> io::Result<()> {
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&tmp)?;
                f.write_all(bytes)?;
                f.set_permissions(std::fs::Permissions::from_mode(mode & 0o777))?;
                f.sync_all()?;
                Ok(())
            };
            if let Err(e) = write() {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
            std::fs::rename(&tmp, &path)
        }

        /// Symlink `target` at `rel` in the staging tree. A target that
        /// would resolve outside the view is refused.
        pub fn symlink(&self, rel: &str, target: &str) -> io::Result<()> {
            let path = self.rel_path(rel)?;
            if !onelf_format::symlink_target_within_root(Path::new(rel), target) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "onelf: the symlink target escapes the view",
                ));
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::os::unix::fs::symlink(target, &path)
        }

        /// Change the content at `rel` in the staging tree by applying
        /// `edit` to the bytes as they are and renaming the result over
        /// the name. The staged name may be a hardlink into the content
        /// store: reading it is shared, and the rename gives the name a
        /// fresh inode, so the store's bytes are never written through.
        pub fn edit_file(
            &self,
            rel: &str,
            edit: impl FnOnce(&[u8]) -> io::Result<Vec<u8>>,
        ) -> io::Result<()> {
            let path = self.rel_path(rel)?;
            let md = std::fs::symlink_metadata(&path)?;
            if !md.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "onelf: only a staged regular file can be edited",
                ));
            }
            let mode = md.permissions().mode();
            let edited = edit(&std::fs::read(&path)?)?;
            self.write_file(rel, &edited, mode)
        }

        /// Make `links` the library links in the package's `dir` inside
        /// the staging tree, and verify they are all there. This is the
        /// only place links are ever installed; a published view's
        /// links never change, so a failed placement fails the build
        /// instead of publishing a half-linked view.
        pub fn place_links(&self, manifest: &Manifest, dir: &str, links: &Links) -> io::Result<()> {
            admit_rel(dir)?;
            std::fs::create_dir_all(self.dir.join(dir))?;
            super::place(manifest, &self.dir, dir, links);
            if super::present(manifest, &self.dir, dir) != *links {
                return Err(io::Error::other(
                    "onelf: the staged view's links are incomplete",
                ));
            }
            Ok(())
        }

        /// [`Staging::place_links`] for a tree staged without a
        /// manifest: the names already staged in `dir` are the tree's
        /// own, so every link named beyond them is placed, nothing the
        /// tree carries is removed, and the result is verified.
        pub fn place_free_links(&self, dir: &str, links: &Links) -> io::Result<()> {
            admit_rel(dir)?;
            std::fs::create_dir_all(self.dir.join(dir))?;
            let carried: std::collections::HashSet<String> = std::fs::read_dir(self.dir.join(dir))
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| e.file_name().to_str().map(String::from))
                .collect();
            let borrowed: std::collections::HashSet<&str> =
                carried.iter().map(String::as_str).collect();
            super::place_excluding(&self.dir, dir, links, &borrowed);
            if super::present_excluding(&self.dir, dir, &borrowed) != *links {
                return Err(io::Error::other(
                    "onelf: the staged view's links are incomplete",
                ));
            }
            Ok(())
        }

        /// Publish the staged tree as `view`: write the expanded
        /// environment input and the configuration record, flush the
        /// whole tree, write the ready record as the
        /// last staging step, and rename the tree onto the view's name
        /// in one atomic step. A tree that fails anywhere is removed.
        fn publish(self, identity: &ViewIdentity, expanded: Option<&[u8]>) -> io::Result<()> {
            let dir = self.dir.clone();
            let view = self.view.clone();
            if let Some(env) = expanded {
                self.write_file(crate::env::VIEW_ENV_FILE, env, 0o644)?;
            }
            ViewRecord::write(&dir, identity, expanded)?;
            sync_tree(&dir)?;
            write_ready(&dir)?;
            std::fs::rename(&dir, &view)?;
            if let Some(parent) = view.parent() {
                let _ = std::fs::File::open(parent).and_then(|d| d.sync_all());
            }
            // The tree now lives at the view's name; the drop that
            // removes staging must not run.
            std::mem::forget(self);
            Ok(())
        }
    }

    /// Remove the staging tree unless the view was published from it.
    impl Drop for Staging {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A unique scratch name beside `path`, for a file renamed over it
    /// once complete.
    fn temp_name(path: &Path) -> PathBuf {
        let seq = crate::cache::TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("f");
        path.with_file_name(format!(".{}.{}.{name}.tmp", std::process::id(), seq))
    }

    /// Write the ready record into a staged tree, replacing anything of
    /// the same name first: a staged file that happens to carry the
    /// record's name must not lend a store hardlink to this write.
    fn write_ready(dir: &Path) -> io::Result<()> {
        let path = dir.join(READY_FILE);
        let _ = std::fs::remove_file(&path);
        let mut f = std::fs::File::create(&path)?;
        f.write_all(&ready_bytes())?;
        f.sync_all()
    }

    /// Flush every file and directory under `root`, so what the rename
    /// publishes is on disk, not in a cache that a power loss would
    /// forget. Bounded in depth; symlinks carry nothing to flush, their
    /// names belong to the parent directory.
    fn sync_tree(root: &Path) -> io::Result<()> {
        let mut pending = vec![(root.to_path_buf(), 0usize)];
        let mut dirs = Vec::new();
        while let Some((dir, depth)) = pending.pop() {
            if depth > MAX_REL_DEPTH {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "onelf: the staged view nests too deeply to publish",
                ));
            }
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                let kind = entry.file_type()?;
                let path = entry.path();
                if kind.is_dir() {
                    pending.push((path, depth + 1));
                } else if kind.is_file() {
                    std::fs::File::open(&path)?.sync_all()?;
                }
            }
            dirs.push(dir);
        }
        for dir in dirs.iter().rev() {
            std::fs::File::open(dir)?.sync_all()?;
        }
        Ok(())
    }

    /// Whether `rel` is a relative path a staging write accepts: non
    /// empty, bounded, without traversal or interior NUL, every
    /// component ordinary.
    fn admit_rel(rel: &str) -> io::Result<()> {
        let refused = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "onelf: the view path is not a bounded relative path",
            )
        };
        if rel.is_empty() || rel.len() > MAX_REL_BYTES || rel.contains('\0') {
            return Err(refused());
        }
        let mut depth = 0usize;
        for component in rel.split('/') {
            if component.is_empty()
                || component == "."
                || component == ".."
                || component.len() > MAX_LINK_NAME
            {
                return Err(refused());
            }
            depth += 1;
            if depth > MAX_REL_DEPTH {
                return Err(refused());
            }
        }
        Ok(())
    }

    /// Remove `path`, file, link, or tree, tolerating absence.
    fn remove_path(path: &Path) -> io::Result<()> {
        match std::fs::symlink_metadata(path) {
            Ok(md) if md.is_dir() => std::fs::remove_dir_all(path),
            Ok(_) => std::fs::remove_file(path),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Remove every staging tree of `view` the store still holds. Only
    /// a creator holding the view's creation mutex may call this: that
    /// mutex proves no live creator of the identity is staging, so
    /// anything left is a dead creator's scratch.
    fn discard_staging(view: &Path) -> io::Result<()> {
        let Some(prefix) = staging_prefix(view) else {
            return Ok(());
        };
        let Some(parent) = view.parent() else {
            return Ok(());
        };
        for entry in std::fs::read_dir(parent)? {
            let entry = entry?;
            if entry.file_name().as_bytes().starts_with(prefix.as_bytes()) {
                remove_path(&entry.path())?;
            }
        }
        Ok(())
    }

    /// The view directory's name for an identity key, `view-<key>`, or
    /// `None` when `key` is not a key [`ViewIdentity`] mints: the name
    /// is one bounded lowercase hex component, so nothing a caller
    /// passes can name anything outside the store.
    pub fn dir_name(key: &str) -> Option<String> {
        let hex = !key.is_empty()
            && key.len() <= KEY_BYTES * 2
            && key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        hex.then(|| format!("view-{key}"))
    }

    /// `Some(())` when `ok`, so admission checks compose with `?`.
    fn admit(ok: bool) -> Option<()> {
        ok.then_some(())
    }

    /// Whether `name` is an entrypoint name a record can carry.
    fn admitted_entry(name: &str) -> bool {
        let bytes = name.as_bytes();
        !bytes.is_empty()
            && bytes.len() <= MAX_ENTRY_NAME
            && bytes.iter().all(|b| !(*b < 0x20 || *b == 0x7f))
    }

    /// Whether `loader` is a loader path a record can carry.
    fn admitted_loader(loader: &OsStr) -> bool {
        let bytes = loader.as_bytes();
        !bytes.is_empty() && bytes.len() <= MAX_LOADER_BYTES && !bytes.contains(&0)
    }

    /// Whether one link's name and target are within a record's bounds,
    /// the name a single path component.
    fn admitted_link(name: &str, target: &OsStr) -> bool {
        let name = name.as_bytes();
        let target = target.as_bytes();
        !name.is_empty()
            && name.len() <= MAX_LINK_NAME
            && !name.contains(&b'/')
            && !name.contains(&0)
            && !target.is_empty()
            && target.len() <= MAX_LINK_TARGET
            && !target.contains(&0)
    }

    /// `env` with every occurrence of the execution root replaced by the
    /// symbolic placeholder, so the identity does not depend on where
    /// the view will live. `None` past the bound or carrying NUL.
    fn normalized_env(env: &[u8], root: &OsStr) -> Option<Vec<u8>> {
        if env.len() > MAX_ENV_BYTES || env.contains(&0) {
            return None;
        }
        let root = root.as_bytes();
        if root.is_empty() || root.contains(&0) {
            return Some(env.to_vec());
        }
        let mut out = Vec::with_capacity(env.len());
        let mut rest = env;
        while let Some(at) = rest.windows(root.len()).position(|w| w == root) {
            out.extend_from_slice(&rest[..at]);
            out.extend_from_slice(ROOT_TOKEN);
            rest = &rest[at + root.len()..];
        }
        out.extend_from_slice(rest);
        Some(out)
    }

    /// The first line of every canonical text and record.
    fn header() -> Vec<u8> {
        format!("onelf-view {LAYOUT}\n").into_bytes()
    }

    /// The canonical text of `inputs`: fixed scalar lines, then every
    /// value length-prefixed, so two different inputs never render one
    /// text. Values keep their bytes; the execution root has already
    /// been replaced symbolically in `env`.
    fn canonical(inputs: &ViewInputs<'_>, selection: &SelectionIdentity, env: &[u8]) -> Vec<u8> {
        let mut c = header();
        c.extend_from_slice(format!("uid {}\n", inputs.uid).as_bytes());
        c.extend_from_slice(format!("package {}\n", crate::cache::hex(inputs.package)).as_bytes());
        match inputs.metadata {
            Some(digest) => {
                c.extend_from_slice(format!("meta {}\n", crate::cache::hex(digest)).as_bytes())
            }
            None => c.extend_from_slice(b"meta -\n"),
        }
        c.extend_from_slice(format!("policy {}\n", inputs.policy.as_str()).as_bytes());
        c.extend_from_slice(
            format!("shadow {}\n", if inputs.host_libc { "y" } else { "n" }).as_bytes(),
        );
        match inputs.loader {
            Some(loader) => push_value(&mut c, b"loader", loader.as_bytes()),
            None => push_absent(&mut c, b"loader"),
        }
        match inputs.entrypoint {
            Some(entry) => push_value(&mut c, b"entry", entry.as_bytes()),
            None => push_absent(&mut c, b"entry"),
        }
        push_value(&mut c, b"pins", selection.canonical().as_bytes());
        c.extend_from_slice(format!("links {}\n", inputs.links.len()).as_bytes());
        for (name, target) in inputs.links {
            let target = target.as_os_str().as_bytes();
            c.extend_from_slice(format!("link {}:", name.len()).as_bytes());
            c.extend_from_slice(name.as_bytes());
            c.push(b'\t');
            c.extend_from_slice(format!("{}:", target.len()).as_bytes());
            c.extend_from_slice(target);
            c.push(b'\n');
        }
        push_value(&mut c, b"env", env);
        c
    }

    /// Append `<tag> <len>:<bytes>` and a newline.
    fn push_value(out: &mut Vec<u8>, tag: &[u8], value: &[u8]) {
        out.extend_from_slice(tag);
        out.push(b' ');
        out.extend_from_slice(value.len().to_string().as_bytes());
        out.push(b':');
        out.extend_from_slice(value);
        out.push(b'\n');
    }

    /// Append `<tag> -` and a newline: the value is absent.
    fn push_absent(out: &mut Vec<u8>, tag: &[u8]) {
        out.extend_from_slice(tag);
        out.extend_from_slice(b" -\n");
    }

    /// The canonical text and the expanded-environment hash a record
    /// carries. `None` for anything this layout did not write, whole and
    /// to the last byte.
    fn parse(bytes: &[u8]) -> Option<(&[u8], Option<String>)> {
        let (rest, end) = parse_canonical(bytes)?;
        let (line, tail) = split_line(rest)?;
        let expanded = match line.strip_prefix(b"expanded ")? {
            b"-" => None,
            hex => Some(hex_digest(hex)?),
        };
        if !tail.is_empty() {
            return None;
        }
        Some((&bytes[..end], expanded))
    }

    /// Walk the canonical structure at the top of `bytes`, returning the
    /// bytes after the final `env` record and where the canonical text
    /// ends. `None` when the shape is not this layout's.
    fn parse_canonical(bytes: &[u8]) -> Option<(&[u8], usize)> {
        let mut rest = bytes.strip_prefix(header().as_slice())?;
        for tag in [
            &b"uid "[..],
            &b"package "[..],
            &b"meta "[..],
            &b"policy "[..],
            &b"shadow "[..],
        ] {
            let (line, tail) = split_line(rest)?;
            line.strip_prefix(tag)?;
            rest = tail;
        }
        for tag in [&b"loader"[..], &b"entry"[..], &b"pins"[..]] {
            rest = value_line(rest, tag)?;
        }
        let (line, mut rest) = split_line(rest)?;
        let count: usize = std::str::from_utf8(line.strip_prefix(b"links ")?)
            .ok()?
            .parse()
            .ok()?;
        if count > MAX_VIEW_LINKS {
            return None;
        }
        for _ in 0..count {
            rest = link_line(rest)?;
        }
        let rest = value_line(rest, b"env")?;
        Some((rest, bytes.len() - rest.len()))
    }

    /// `<tag> <len>:<bytes>` or `<tag> -`, and a newline, consumed from
    /// `rest`.
    fn value_line<'a>(rest: &'a [u8], tag: &[u8]) -> Option<&'a [u8]> {
        let after = rest.strip_prefix(tag)?;
        if let Some(tail) = after.strip_prefix(b" -\n") {
            return Some(tail);
        }
        let after = after.strip_prefix(b" ")?;
        let colon = after.iter().position(|b| *b == b':')?;
        let len: usize = std::str::from_utf8(&after[..colon]).ok()?.parse().ok()?;
        let tail = after.get(colon + 1 + len..)?;
        tail.strip_prefix(b"\n")
    }

    /// One `link <len>:<name>\t<len>:<target>` and a newline, consumed
    /// from `rest`.
    fn link_line(rest: &[u8]) -> Option<&[u8]> {
        let after = rest.strip_prefix(b"link ")?;
        let colon = after.iter().position(|b| *b == b':')?;
        let len: usize = std::str::from_utf8(&after[..colon]).ok()?.parse().ok()?;
        let after = after.get(colon + 1 + len..)?;
        let after = after.strip_prefix(b"\t")?;
        let colon = after.iter().position(|b| *b == b':')?;
        let len: usize = std::str::from_utf8(&after[..colon]).ok()?.parse().ok()?;
        let tail = after.get(colon + 1 + len..)?;
        tail.strip_prefix(b"\n")
    }

    fn split_line(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
        let at = bytes.iter().position(|b| *b == b'\n')?;
        Some((&bytes[..at], &bytes[at + 1..]))
    }

    /// The full lowercase digest `bytes` spells, or `None`.
    fn hex_digest(bytes: &[u8]) -> Option<String> {
        let text = std::str::from_utf8(bytes).ok()?;
        (text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
            .then(|| text.to_string())
    }

    impl fmt::Debug for ViewIdentity {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("ViewIdentity")
                .field("key", &self.key)
                .field("uid", &self.uid)
                .finish_non_exhaustive()
        }
    }

    impl fmt::Debug for ViewRecord {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("ViewRecord")
                .field("bytes", &self.canonical.len())
                .field("expanded_env", &self.expanded_env.as_ref().map(|_| "set"))
                .finish()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use onelf_format::ManifestHeader;
        use std::path::PathBuf;

        use super::super::{place, present};

        fn temp_dir(tag: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!("onelf-view-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        /// A pin named `label` whose build is pinned by the digest `n`
        /// names.
        fn pin(label: &str, n: u8) -> ActivePin {
            ActivePin {
                label: label.to_string(),
                hash: format!("{n:02x}").repeat(32),
            }
        }

        /// The links of a typical view: one winner from the farm.
        fn sample_links() -> Links {
            Links::from([(
                "libfoo.so.1".to_string(),
                PathBuf::from("/host/lib/libfoo.so.1"),
            )])
        }

        /// The environment input of a typical view, naming `root` the
        /// way the generated mappings do.
        fn sample_env(root: &OsStr) -> Vec<u8> {
            format!(
                "GDK_PIXBUF_MODULE_FILE={}/lib/loaders.cache\nR {}:restored\n",
                root.to_string_lossy(),
                root.to_string_lossy()
            )
            .into_bytes()
        }

        /// A leaked pins slice the identity can borrow for the test's
        /// lifetime.
        fn leaked(pins: Vec<ActivePin>) -> &'static [ActivePin] {
            Box::leak(pins.into_boxed_slice())
        }

        /// A leaked package or metadata identity the identity can
        /// borrow for the test's lifetime.
        fn leaked_id(n: u8) -> &'static [u8; 32] {
            Box::leak(Box::new([n; 32]))
        }

        /// The typical view's inputs, rooted at `root`.
        fn inputs<'a>(root: &'a OsStr, env: &'a [u8], links: &'a Links) -> ViewInputs<'a> {
            ViewInputs {
                uid: 1000,
                package: leaked_id(1),
                metadata: Some(leaked_id(3)),
                pins: leaked(vec![pin("gtk", 1)]),
                policy: HostLibsPolicy::Auto,
                host_libc: true,
                loader: None,
                entrypoint: Some("app"),
                links,
                runtime_env: env,
                root,
            }
        }

        /// The identity of the typical view rooted at `root`.
        fn sample(root: &OsStr) -> (ViewIdentity, Vec<u8>, Links) {
            let env = sample_env(root);
            let links = sample_links();
            let inputs = inputs(root, &env, &links);
            (ViewIdentity::new(&inputs).unwrap(), env, links)
        }

        /// `bytes` with the first occurrence of `from` replaced by `to`,
        /// which must be the same length.
        fn swap(bytes: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
            let at = bytes
                .windows(from.len())
                .position(|w| w == from)
                .expect("the bytes to tamper are present");
            let mut out = bytes.to_vec();
            out[at..at + to.len()].copy_from_slice(to);
            out
        }

        /// One change to any isolating input gives another identity and
        /// another key, even when the selection is untouched.
        #[test]
        fn every_isolating_input_changes_the_identity() {
            let root = OsStr::new("/run/views/a");
            let (base, env, links) = sample(root);
            let other_env = b"GDK_PIXBUF_MODULE_FILE=/elsewhere/loaders.cache\n".to_vec();
            let mut other_links = sample_links();
            other_links.insert("libbar.so.1".into(), PathBuf::from("/set/lib/libbar.so.1"));
            let mut retargeted = sample_links();
            *retargeted.get_mut("libfoo.so.1").unwrap() = PathBuf::from("/host/lib64/libfoo.so.1");
            let mut loader_root = root.to_os_string();
            loader_root.push("/ld");
            let variants: Vec<(&str, ViewInputs)> = vec![
                (
                    "uid",
                    ViewInputs {
                        uid: 1001,
                        ..inputs(root, &env, &links)
                    },
                ),
                (
                    "package",
                    ViewInputs {
                        package: leaked_id(2),
                        ..inputs(root, &env, &links)
                    },
                ),
                (
                    "metadata",
                    ViewInputs {
                        metadata: Some(leaked_id(4)),
                        ..inputs(root, &env, &links)
                    },
                ),
                (
                    "no metadata",
                    ViewInputs {
                        metadata: None,
                        ..inputs(root, &env, &links)
                    },
                ),
                (
                    "pins",
                    ViewInputs {
                        pins: leaked(vec![pin("gtk", 2)]),
                        ..inputs(root, &env, &links)
                    },
                ),
                (
                    "pin subset",
                    ViewInputs {
                        pins: leaked(vec![]),
                        ..inputs(root, &env, &links)
                    },
                ),
                (
                    "links policy",
                    ViewInputs {
                        policy: HostLibsPolicy::Never,
                        ..inputs(root, &env, &links)
                    },
                ),
                (
                    "libc policy",
                    ViewInputs {
                        host_libc: false,
                        ..inputs(root, &env, &links)
                    },
                ),
                (
                    "loader",
                    ViewInputs {
                        loader: Some(loader_root.as_os_str()),
                        ..inputs(root, &env, &links)
                    },
                ),
                (
                    "entrypoint",
                    ViewInputs {
                        entrypoint: Some("cli"),
                        ..inputs(root, &env, &links)
                    },
                ),
                (
                    "effective links",
                    ViewInputs {
                        links: &other_links,
                        ..inputs(root, &env, &links)
                    },
                ),
                (
                    "retargeted link",
                    ViewInputs {
                        links: &retargeted,
                        ..inputs(root, &env, &links)
                    },
                ),
                (
                    "caller environment",
                    ViewInputs {
                        runtime_env: &other_env,
                        ..inputs(root, &other_env, &links)
                    },
                ),
            ];

            for (what, inputs) in &variants {
                let other = ViewIdentity::new(inputs).expect(what);
                assert_ne!(base, other, "{what} must give another identity");
                assert_ne!(base.key(), other.key(), "{what} must give another key");
            }
        }

        /// Inputs that differ only in presentation share one identity:
        /// pin list order and pin hash case.
        #[test]
        fn equivalent_presentations_share_one_identity() {
            let root = OsStr::new("/run/views/a");
            let (base, env, links) = sample(root);
            let mut both_links = sample_links();
            both_links.insert("libbar.so.1".into(), PathBuf::from("/set/lib/libbar.so.1"));
            let mut upper = pin("qt", 2);
            upper.hash = upper.hash.to_ascii_uppercase();
            let reordered = leaked(vec![pin("qt", 2), pin("gtk", 1)]);
            let lower = leaked(vec![pin("gtk", 1), pin("qt", 2)]);
            let cased = leaked(vec![pin("gtk", 1), upper]);

            let one = ViewIdentity::new(&ViewInputs {
                pins: reordered,
                links: &both_links,
                ..inputs(root, &env, &links)
            })
            .unwrap();
            let two = ViewIdentity::new(&ViewInputs {
                pins: lower,
                links: &both_links,
                ..inputs(root, &env, &links)
            })
            .unwrap();
            let three = ViewIdentity::new(&ViewInputs {
                pins: cased,
                links: &both_links,
                ..inputs(root, &env, &links)
            })
            .unwrap();
            assert_ne!(base, one, "a wider selection is another view");
            assert_eq!(one, two, "list order does not matter");
            assert_eq!(one, three, "hash case does not matter");
            assert_eq!(one.key(), three.key());
        }

        /// The identity is a pure function of its inputs: nothing is
        /// created or read on the way, and the execution root never
        /// appears in the canonical text, so a key cannot depend
        /// recursively on the name of the directory it will name.
        #[test]
        fn the_identity_is_a_pure_function_of_its_inputs() {
            let scratch = temp_dir("pure");
            let root = scratch.join("tree");
            let root = root.as_os_str();
            let env = sample_env(root);
            let links = sample_links();
            let first = ViewIdentity::new(&inputs(root, &env, &links)).unwrap();
            assert_eq!(
                std::fs::read_dir(&scratch).unwrap().count(),
                0,
                "computing the identity must leave nothing behind"
            );

            // What the root path comes to name later cannot matter: the
            // canonical text does not carry it.
            std::fs::create_dir_all(scratch.join("tree")).unwrap();
            std::fs::write(scratch.join("tree/libfoo.so.1"), b"bytes").unwrap();
            let second = ViewIdentity::new(&inputs(root, &env, &links)).unwrap();
            assert_eq!(first, second);

            let other_root = OsStr::new("/mnt/elsewhere/tree");
            let other_env = sample_env(other_root);
            let other = ViewIdentity::new(&inputs(other_root, &other_env, &links)).unwrap();
            assert_eq!(first, other, "only the root's substitutions differ");
            let canonical = first.canonical();
            for root in [root, other_root] {
                let root = root.as_bytes();
                assert!(
                    !canonical.windows(root.len()).any(|w| w == root),
                    "the canonical text must not name the execution root"
                );
            }
            let _ = std::fs::remove_dir_all(&scratch);
        }

        /// The record round-trips: what was written reads back bound to
        /// its identity, the expanded-environment hash is carried, the
        /// write is atomic, and a reader can answer the configuration
        /// from the record without recomputing it.
        #[test]
        fn a_record_round_trips_and_carries_the_configuration() {
            let root = OsStr::new("/run/views/rt");
            let (identity, env, links) = sample(root);
            let dir = temp_dir("roundtrip");

            ViewRecord::write(&dir, &identity, Some(&env)).unwrap();
            let names = |dir: &Path| {
                let mut out: Vec<String> = std::fs::read_dir(dir)
                    .unwrap()
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect();
                out.sort();
                out
            };
            assert_eq!(names(&dir), [CONFIGURATION_FILE.to_string()]);

            let record = ViewRecord::load(&dir, &identity).unwrap();
            assert_eq!(record.canonical.as_slice(), identity.canonical());
            assert_eq!(
                record.expanded_env.as_deref(),
                Some(blake3::hash(&env).to_hex().to_string()).as_deref()
            );

            // The configuration answers what was built without
            // recomputing: every input is on the record.
            let text = String::from_utf8_lossy(record.canonical.as_slice());
            assert!(text.contains("uid 1000"));
            assert!(text.contains("policy auto"));
            assert!(text.contains("entry 3:app"));
            assert!(text.contains("libfoo.so.1"));

            // A rewrite replaces the record whole, and an unbuilt view
            // records no expanded hash.
            ViewRecord::write(&dir, &identity, None).unwrap();
            assert_eq!(names(&dir), [CONFIGURATION_FILE.to_string()]);
            assert_eq!(
                ViewRecord::load(&dir, &identity)
                    .unwrap()
                    .expanded_env
                    .as_deref(),
                None
            );

            // A record of another identity is not this one's. Another
            // execution root is not another identity: the root is
            // normalized away, so only another input separates them.
            let other_identity = ViewIdentity::new(&ViewInputs {
                uid: 1001,
                ..inputs(root, &env, &links)
            })
            .unwrap();
            assert_ne!(identity, other_identity);
            assert_eq!(ViewRecord::load(&dir, &other_identity), None);
            assert_eq!(ViewRecord::load(&dir.join("missing"), &identity), None);

            let _ = std::fs::remove_dir_all(&dir);
        }

        /// A record of another layout version, or hostile or damaged
        /// state of any shape, is a miss with nothing adopted.
        #[test]
        fn hostile_or_foreign_records_are_misses() {
            let root = OsStr::new("/run/views/hostile");
            let (identity, env, _) = sample(root);
            let dir = temp_dir("hostile");
            ViewRecord::write(&dir, &identity, Some(&env)).unwrap();
            let path = dir.join(CONFIGURATION_FILE);
            let good = std::fs::read(&path).unwrap();
            let miss = |bytes: &[u8]| {
                std::fs::write(&path, bytes).unwrap();
                assert_eq!(ViewRecord::load(&dir, &identity), None);
            };

            miss(b"");
            miss(b"onelf-view 2\nuid 1000\n");
            miss(b"onelf-view 1");
            miss(&good[..good.len() / 2]);
            miss(&good[..good.len() - 1]);
            let mut trailing = good.clone();
            trailing.push(b'\n');
            miss(&trailing);
            miss(&swap(&good, b"uid 1000", b"uid 1001"));
            miss(&swap(&good, b"shadow y", b"shadow n"));
            miss(&swap(&good, b"links 1", b"links 2"));
            let mut bad_expanded = good.clone();
            let at = bad_expanded
                .windows(b"expanded ".len())
                .position(|w| w == b"expanded ")
                .expect("the expanded record is present");
            bad_expanded[at + b"expanded ".len()] = b'z';
            miss(&bad_expanded);
            let oversized = vec![b'x'; RECORD_MAX_BYTES as usize + 1];
            miss(&oversized);

            // A directory where the record should be is not a record.
            std::fs::remove_file(&path).unwrap();
            std::fs::create_dir_all(&path).unwrap();
            assert_eq!(ViewRecord::load(&dir, &identity), None);

            std::fs::remove_dir_all(&path).unwrap();
            std::fs::write(&path, &good).unwrap();
            assert!(
                ViewRecord::load(&dir, &identity).is_some(),
                "the intact record is still a hit"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// Nothing a diagnostic can render carries an environment
        /// value: the identity and the record print their key and
        /// scalar fields only, and failures of the record write name no
        /// value either.
        #[test]
        fn diagnostics_never_echo_environment_values() {
            let secret = "SECRET_TOKEN=do-not-echo";
            let restored = "caller-original-9f2c";
            let root = OsStr::new("/run/views/quiet");
            let env = format!("{secret}\nHOME_BACKUP={restored}\n").into_bytes();
            let links = sample_links();
            let identity = ViewIdentity::new(&inputs(root, &env, &links)).unwrap();

            let rendered = format!("{identity:?}");
            assert!(!rendered.contains(secret));
            assert!(!rendered.contains(restored));
            assert!(!rendered.contains("GDK_PIXBUF_MODULE_FILE"));

            let dir = temp_dir("quiet");
            ViewRecord::write(&dir, &identity, Some(&env)).unwrap();
            let record = ViewRecord::load(&dir, &identity).unwrap();
            let rendered = format!("{record:?}");
            assert!(!rendered.contains(secret));
            assert!(!rendered.contains(restored));

            // A write that fails names its reason, never a value.
            let blocked = dir.join("f");
            std::fs::write(&blocked, b"x").unwrap();
            let err = ViewRecord::write(&blocked, &identity, Some(&env)).unwrap_err();
            let rendered = err.to_string();
            assert!(!rendered.contains(secret));
            assert!(!rendered.contains(restored));
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// An input outside what a record can carry refuses the
        /// identity, at the bound and past it, so a launch that cannot
        /// be identified is never cached under a guessed key.
        #[test]
        fn inputs_outside_the_bounds_refuse_an_identity() {
            let root = OsStr::new("/run/views/bounds");
            let (base, env, links) = sample(root);
            let refused = |inputs: &ViewInputs| ViewIdentity::new(inputs).is_none();

            let bad_pin = ActivePin {
                label: "a\tb".to_string(),
                hash: pin("gtk", 1).hash,
            };
            assert!(refused(&ViewInputs {
                pins: leaked(vec![bad_pin]),
                ..inputs(root, &env, &links)
            }));

            for entry in ["", &"g".repeat(129), "a\tb", "a\nb"] {
                assert!(
                    refused(&ViewInputs {
                        entrypoint: Some(entry),
                        ..inputs(root, &env, &links)
                    }),
                    "entrypoint out of bounds is refused"
                );
            }

            let mut long_loader = root.to_os_string();
            long_loader.push("/");
            long_loader.push("l".repeat(MAX_LOADER_BYTES));
            let cases: [Option<&OsStr>; 3] = [
                Some(OsStr::new("")),
                Some(OsStr::new("/ld\0")),
                Some(long_loader.as_os_str()),
            ];
            for loader in cases {
                assert!(
                    refused(&ViewInputs {
                        loader,
                        ..inputs(root, &env, &links)
                    }),
                    "a loader out of bounds is refused"
                );
            }

            let long_name = "n".repeat(MAX_LINK_NAME + 1);
            for bad in [
                Links::from([("a/b".to_string(), PathBuf::from("/t"))]),
                Links::from([("".to_string(), PathBuf::from("/t"))]),
                Links::from([(long_name.clone(), PathBuf::from("/t"))]),
                Links::from([("n".to_string(), PathBuf::from(""))]),
                Links::from([("n".to_string(), PathBuf::from("/t\0"))]),
            ] {
                assert!(
                    refused(&ViewInputs {
                        links: &bad,
                        ..inputs(root, &env, &links)
                    }),
                    "a link out of bounds is refused"
                );
            }
            let mut crowded = sample_links();
            for i in 0..MAX_VIEW_LINKS {
                crowded.insert(format!("lib{i}.so.1"), PathBuf::from("/t"));
            }
            assert!(refused(&ViewInputs {
                links: &crowded,
                ..inputs(root, &env, &links)
            }));

            let nul_env = vec![b'a', 0, b'b'];
            let long_env = vec![b'x'; MAX_ENV_BYTES + 1];
            for env in [&nul_env, &long_env] {
                assert!(
                    refused(&ViewInputs {
                        runtime_env: env,
                        ..inputs(root, env, &links)
                    }),
                    "an environment input out of bounds is refused"
                );
            }

            // At the bounds everything is admitted.
            let mut at_bound = Links::new();
            for i in 0..MAX_VIEW_LINKS - 1 {
                at_bound.insert(format!("lib{i}.so.1"), PathBuf::from("/t"));
            }
            at_bound.insert("n".repeat(MAX_LINK_NAME), PathBuf::from("/t"));
            let at_env = vec![b'x'; MAX_ENV_BYTES];
            let mut at_loader = root.to_os_string();
            at_loader.push("/");
            let room = MAX_LOADER_BYTES - root.len() - 1;
            at_loader.push("l".repeat(room));
            let admitted = ViewIdentity::new(&ViewInputs {
                loader: Some(at_loader.as_os_str()),
                links: &at_bound,
                runtime_env: &at_env,
                ..inputs(root, &at_env, &links)
            });
            assert!(admitted.is_some(), "exactly at the bounds is admitted");
            assert_ne!(admitted.unwrap(), base);
        }

        /// A manifest with no entries, enough for link placement and
        /// verification on a directory the package carries nothing in.
        fn empty_manifest() -> Manifest {
            Manifest {
                header: ManifestHeader {
                    version: 3,
                    entry_count: 0,
                    string_table_size: 0,
                    entrypoint_count: 0,
                    default_entrypoint: 0,
                    lib_dir_count: 0,
                    name_offset: 0,
                    package_id: [0; 32],
                    prefix_hash: [0; 32],
                    footer_hash: [0; 32],
                },
                entrypoints: Vec::new(),
                entries: Vec::new(),
                lib_dir_offsets: Vec::new(),
                string_table: Vec::new(),
            }
        }

        /// The identity every fixture shares. It is a pure function of
        /// fixed constants, so the driver and every worker process
        /// reconstruct the same key and race for one view.
        fn fixture_identity() -> ViewIdentity {
            sample(OsStr::new("/onelf/fixtures/view")).0
        }

        /// The store-relative name of a fixture's view.
        fn view_path(store: &Path, identity: &ViewIdentity) -> PathBuf {
            store.join(format!("view-{}", identity.key()))
        }

        /// The in-use lease of `identity`'s view in `store`, held shared
        /// the way a launch holds it before it builds or reuses the view.
        fn lease(store: &Path, identity: &ViewIdentity) -> std::fs::File {
            use rustix::fs::FlockOperation;
            std::fs::create_dir_all(store).unwrap();
            let path = lease_lock_path(&view_path(store, identity)).unwrap();
            // Close-on-exec, unlike a launch's: a test worker another thread
            // spawns must not inherit the claim.
            let file = std::fs::File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .unwrap();
            rustix::fs::flock(&file, FlockOperation::LockShared).unwrap();
            file
        }

        /// The prefix every staging tree of the fixture's view starts
        /// with, and the staging entries the store still holds.
        fn staging_entries(store: &Path, identity: &ViewIdentity) -> Vec<PathBuf> {
            let prefix = format!("view-{}{}", identity.key(), STAGING_SEP);
            let mut out = Vec::new();
            if let Ok(entries) = std::fs::read_dir(store) {
                for entry in entries.flatten() {
                    if entry.file_name().as_bytes().starts_with(prefix.as_bytes()) {
                        out.push(entry.path());
                    }
                }
            }
            out.sort();
            out
        }

        /// Append one line to the build journal at `path`.
        fn journal(path: &Path, line: &str) {
            use std::io::Write as _;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap()
                .write_all(format!("{line}\n").as_bytes())
                .unwrap();
        }

        /// The environment input the fixture's build records as
        /// expanded into the view.
        fn fixture_env() -> Vec<u8> {
            b"FIXTURE_ENV=1\n".to_vec()
        }

        /// The build every fixture runs: content, a package symlink,
        /// the view's library links, the journal line, and the expanded
        /// environment input. Deterministic apart from `tag`, which the
        /// staged entrypoint carries so a test can tell whose build
        /// won.
        fn fixture_build(
            staged: &Staging,
            tag: &str,
            journal_path: &Path,
        ) -> io::Result<Option<Vec<u8>>> {
            staged.write_file("bin/app", format!("app {tag}\n").as_bytes(), 0o755)?;
            staged.write_file("lib/native.so", b"native-bytes", 0o644)?;
            staged.symlink("bin/rel", "app")?;
            staged.place_links(&empty_manifest(), "lib", &sample_links())?;
            journal(journal_path, &format!("build {tag}"));
            Ok(Some(fixture_env()))
        }

        /// Whether a view directory shows a completely published
        /// fixture: the ready record, the bound record, and every file
        /// and link the build staged, carrying `tag`'s build.
        fn assert_view_complete(view: &Path, identity: &ViewIdentity, tag: &str) {
            assert!(ready(view), "the ready record marks the view published");
            let record = ViewRecord::load(view, identity)
                .expect("the published record binds the view to its identity");
            assert_eq!(
                record.expanded_env.as_deref(),
                Some(blake3::hash(&fixture_env()).to_hex().to_string()).as_deref()
            );
            assert_eq!(
                std::fs::read(view.join("bin/app")).unwrap(),
                format!("app {tag}\n").into_bytes(),
                "the winning build's content is what published"
            );
            assert_eq!(
                std::fs::read(view.join("lib/native.so")).unwrap(),
                b"native-bytes".to_vec()
            );
            assert_eq!(
                std::fs::read_link(view.join("lib/libfoo.so.1")).unwrap(),
                PathBuf::from("/host/lib/libfoo.so.1")
            );
            assert_eq!(
                std::fs::read_link(view.join("bin/rel")).unwrap(),
                PathBuf::from("app")
            );
        }

        /// Whether the file at `path` appears within `secs`.
        fn wait_for(path: &Path, secs: u64) -> bool {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
            while std::time::Instant::now() < deadline {
                if path.exists() {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            false
        }

        /// What a worker reads from its environment: the coordination
        /// directory, the store, its index in the race, and the tag its
        /// build stamps.
        fn worker_env() -> (PathBuf, PathBuf, usize, String) {
            let read = |name: &str| {
                std::env::var(name).unwrap_or_else(|_| panic!("the driver passes {name}"))
            };
            (
                PathBuf::from(read("ONELF_VIEW_DIR")),
                PathBuf::from(read("ONELF_VIEW_STORE")),
                read("ONELF_VIEW_INDEX").parse().expect("a worker index"),
                read("ONELF_VIEW_TAG"),
            )
        }

        /// Start one worker of this binary, alone in a process of its
        /// own, with the fixture's environment.
        fn spawn_worker(
            exe: &Path,
            worker: &str,
            dir: &Path,
            store: &Path,
            index: usize,
            tag: &str,
        ) -> std::process::Child {
            std::process::Command::new(exe)
                .arg(format!("links::view::tests::{worker}"))
                .args(["--exact", "--ignored"])
                .env("ONELF_VIEW_DIR", dir)
                .env("ONELF_VIEW_STORE", store)
                .env("ONELF_VIEW_INDEX", index.to_string())
                .env("ONELF_VIEW_TAG", tag)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("spawn the worker process")
        }

        /// Wait, bounded, for one worker to finish; a failure carries
        /// whatever it printed.
        fn wait_for_worker(mut child: std::process::Child, worker: &str) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
            let status = loop {
                match child.try_wait().expect("poll the worker") {
                    Some(status) => break status,
                    None if std::time::Instant::now() >= deadline => {
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!("worker {worker} did not finish in 90s");
                    }
                    None => std::thread::sleep(std::time::Duration::from_millis(20)),
                }
            };
            let mut out = String::new();
            let mut err = String::new();
            if let Some(mut pipe) = child.stdout.take() {
                let _ = std::io::Read::read_to_string(&mut pipe, &mut out);
            }
            if let Some(mut pipe) = child.stderr.take() {
                let _ = std::io::Read::read_to_string(&mut pipe, &mut err);
            }
            assert!(
                status.success(),
                "worker {worker} failed: {status}\n{out}\n{err}"
            );
        }

        /// A build journal that counts builds in this process, so a
        /// test can tell a reuse from a rebuild.
        #[derive(Clone, Default)]
        struct BuildCount(std::rc::Rc<std::cell::Cell<u32>>);

        impl BuildCount {
            fn value(&self) -> u32 {
                self.0.get()
            }

            /// The fixture build closure that counts itself.
            fn build<'a>(
                &self,
                tag: &'a str,
                journal_path: &'a Path,
            ) -> impl FnOnce(&Staging) -> io::Result<Option<Vec<u8>>> + 'a {
                let count = self.0.clone();
                move |staged: &Staging| {
                    count.set(count.get() + 1);
                    fixture_build(staged, tag, journal_path)
                }
            }
        }

        /// The published view is exactly what staging became: complete
        /// content, links, record, and ready record, with no staging
        /// left behind, and a second call reuses instead of rebuilding.
        #[test]
        fn a_view_publishes_only_from_complete_staging() {
            let dir = temp_dir("publish");
            let store = dir.join("store");
            let identity = fixture_identity();
            let builds = BuildCount::default();

            let published = ensure_view(
                &store,
                &identity,
                &lease(&store, &identity),
                builds.build("first", &dir.join("journal")),
            )
            .expect("the view publishes");
            assert_eq!(published.dir(), view_path(&store, &identity));
            assert_view_complete(published.dir(), &identity, "first");
            assert_eq!(builds.value(), 1);
            assert!(staging_entries(&store, &identity).is_empty());
            assert_eq!(
                std::fs::symlink_metadata(published.dir())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700,
                "the published view stays private"
            );

            // A ready view with an intact record is reused untouched:
            // no second build, no second publication, same content.
            let reused = ensure_view(
                &store,
                &identity,
                &lease(&store, &identity),
                builds.build("second", &dir.join("journal")),
            )
            .expect("the view is reused");
            assert_eq!(reused.dir(), published.dir());
            assert_eq!(builds.value(), 1, "reuse never rebuilds");
            assert_view_complete(published.dir(), &identity, "first");

            let _ = std::fs::remove_dir_all(&dir);
        }

        /// Simultaneous creation of one identity observes one complete
        /// view: the creators serialize on the creation mutex, one
        /// stages and publishes, the losers reuse the winner's view.
        #[test]
        fn concurrent_creators_of_one_identity_serialize() {
            let _forking = crate::test_locks::forking();
            let dir = temp_dir("concurrent");
            let store = dir.join("store");
            let exe = std::env::current_exe().expect("the test binary's path");
            let workers = 4;

            let mut children = Vec::new();
            for index in 0..workers {
                children.push(spawn_worker(
                    &exe,
                    "worker_concurrent_creators",
                    &dir,
                    &store,
                    index,
                    &format!("w{index}"),
                ));
            }
            for index in 0..workers {
                assert!(
                    wait_for(&dir.join(format!("up.{index}")), 30),
                    "worker {index} never reached the race"
                );
            }
            std::fs::write(dir.join("go"), b"").expect("start the race");
            for (index, child) in children.into_iter().enumerate() {
                wait_for_worker(child, &format!("creator {index}"));
            }

            let identity = fixture_identity();
            let journal = std::fs::read_to_string(dir.join("journal")).expect("the journal");
            assert_eq!(
                journal
                    .lines()
                    .filter(|line| line.starts_with("build "))
                    .count(),
                1,
                "exactly one creator staged and published"
            );
            let winner = journal
                .lines()
                .find_map(|line| line.strip_prefix("build "))
                .expect("the winning build is journaled");
            assert_view_complete(&view_path(&store, &identity), &identity, winner);
            assert!(
                staging_entries(&store, &identity).is_empty(),
                "the losers left no staging behind"
            );
            for index in 0..workers {
                assert!(
                    dir.join(format!("ok.{index}")).exists(),
                    "creator {index} observed a complete view"
                );
            }

            let _ = std::fs::remove_dir_all(&dir);
        }

        /// Runs in its own process: races for one view beside the other
        /// workers, and accepts the view only when it is complete.
        #[test]
        #[ignore = "driven by concurrent_creators_of_one_identity_serialize, in a process of its own"]
        fn worker_concurrent_creators() {
            let (dir, store, index, tag) = worker_env();
            let identity = fixture_identity();
            let _ = std::fs::write(dir.join(format!("up.{index}")), b"");
            assert!(
                wait_for(&dir.join("go"), 30),
                "the driver never started the race"
            );

            let published = ensure_view(
                &store,
                &identity,
                &lease(&store, &identity),
                |staged: &Staging| fixture_build(staged, &tag, &dir.join("journal")),
            )
            .expect("the view");

            // Whatever process won, this creator observed a complete
            // published view, never staging.
            assert!(ready(published.dir()), "the view is published");
            assert!(ViewRecord::load(published.dir(), &identity).is_some());
            assert!(published.dir().join("bin/app").is_file());
            assert!(published.dir().join("lib/libfoo.so.1").is_symlink());
            assert!(!staged_view_seen(published.dir()));
            let _ = std::fs::write(dir.join(format!("ok.{index}")), b"");
        }

        /// A staging tree of this identity, if one is still around.
        fn staged_view_seen(view: &Path) -> bool {
            let Some(parent) = view.parent() else {
                return false;
            };
            let Some(name) = view.file_name().and_then(|n| n.to_str()) else {
                return false;
            };
            let prefix = format!("{name}{STAGING_SEP}");
            std::fs::read_dir(parent)
                .map(|entries| {
                    entries
                        .flatten()
                        .any(|e| e.file_name().as_bytes().starts_with(prefix.as_bytes()))
                })
                .unwrap_or(false)
        }

        /// A creator killed mid-build leaves only scratch: nothing is
        /// adopted, and the next creator discards the interrupted
        /// staging and publishes a complete view of its own.
        #[test]
        fn an_interrupted_creator_leaves_nothing_adoptable() {
            let _forking = crate::test_locks::forking();
            let dir = temp_dir("interrupted");
            let store = dir.join("store");
            let exe = std::env::current_exe().expect("the test binary's path");
            let identity = fixture_identity();
            let view = view_path(&store, &identity);

            let child = spawn_worker(
                &exe,
                "worker_interrupted_creator",
                &dir,
                &store,
                0,
                "doomed",
            );
            assert!(
                wait_for(&dir.join("started"), 30),
                "the worker never reached its build"
            );
            let leftovers = staging_entries(&store, &identity);
            assert_eq!(
                leftovers.len(),
                1,
                "the interrupted creator left exactly its staging tree"
            );
            assert_eq!(
                std::fs::read(leftovers[0].join("bin/app")).unwrap(),
                b"app doomed\n".to_vec(),
                "the staging tree is the killed creator's partial work"
            );
            assert!(!ready(&view), "nothing was published");

            let mut child = child;
            child.kill().expect("stop the interrupted creator");

            // The next creator refuses the interrupted state: it
            // rebuilds, and the dead creator's staging is reclaimed.
            // Reap the killed worker first, so its descriptors and its
            // locks are gone before the rebuild starts.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            loop {
                match child.try_wait().expect("poll the stopped worker") {
                    Some(_) => break,
                    None if std::time::Instant::now() >= deadline => {
                        panic!("the killed worker was never reaped");
                    }
                    None => std::thread::sleep(std::time::Duration::from_millis(20)),
                }
            }

            // The next creator refuses the interrupted state: it
            // rebuilds, and the dead creator's staging is reclaimed.
            let builds = BuildCount::default();
            let published = ensure_view(
                &store,
                &identity,
                &lease(&store, &identity),
                builds.build("first", &dir.join("journal")),
            )
            .expect("the rebuilt view");
            assert_view_complete(published.dir(), &identity, "first");
            assert_eq!(builds.value(), 1);
            assert!(
                staging_entries(&store, &identity).is_empty(),
                "the dead creator's staging is reclaimed by the next creator"
            );
            let journal = std::fs::read_to_string(dir.join("journal")).unwrap();
            assert!(
                journal.lines().any(|line| line == "partial doomed"),
                "the killed creator did reach its build"
            );
            assert_eq!(
                journal
                    .lines()
                    .filter(|line| line.starts_with("build "))
                    .count(),
                1,
                "the interrupted attempt never published"
            );

            let _ = std::fs::remove_dir_all(&dir);
        }

        /// Runs in its own process: stages a partial tree, announces it,
        /// and stays there until the driver stops it mid-build.
        #[test]
        #[ignore = "driven by an_interrupted_creator_leaves_nothing_adoptable, in a process of its own"]
        fn worker_interrupted_creator() {
            let (dir, store, _index, tag) = worker_env();
            let identity = fixture_identity();
            let _ = ensure_view(
                &store,
                &identity,
                &lease(&store, &identity),
                |staged: &Staging| {
                    staged.write_file("bin/app", format!("app {tag}\n").as_bytes(), 0o755)?;
                    journal(&dir.join("journal"), &format!("partial {tag}"));
                    let _ = std::fs::write(dir.join("started"), b"");
                    for _ in 0..600 {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    Ok(None)
                },
            );
        }

        /// A missing, truncated, foreign-versioned, or hostile ready
        /// record, and a damaged or foreign configuration record, are
        /// all misses: the view is rebuilt from staging, never adopted
        /// half read.
        #[test]
        fn a_damaged_view_is_a_miss_that_rebuilds() {
            let dir = temp_dir("damaged");
            let store = dir.join("store");
            let identity = fixture_identity();
            let view = view_path(&store, &identity);
            let ready_path = view.join(READY_FILE);
            let builds = BuildCount::default();

            let published = ensure_view(
                &store,
                &identity,
                &lease(&store, &identity),
                builds.build("first", &dir.join("journal")),
            )
            .expect("the first publication");
            assert_view_complete(published.dir(), &identity, "first");
            assert_eq!(builds.value(), 1);
            let intact = std::fs::read(&ready_path).unwrap();

            // Every mangling below must end in a rebuild whose view is
            // whole again.
            let rebuild = |mangle: &dyn Fn()| {
                mangle();
                let rebuilt = ensure_view(
                    &store,
                    &identity,
                    &lease(&store, &identity),
                    builds.build("again", &dir.join("journal")),
                )
                .expect("the rebuild");
                assert_view_complete(rebuilt.dir(), &identity, "again");
                assert_eq!(
                    std::fs::read(&ready_path).unwrap(),
                    ready_bytes(),
                    "the rebuilt ready record is this layout's"
                );
            };

            std::fs::remove_file(&ready_path).unwrap();
            rebuild(&|| {});
            assert_eq!(builds.value(), 2, "a missing ready record is a miss");

            let mut truncated = intact.clone();
            truncated.pop();
            rebuild(&|| std::fs::write(&ready_path, &truncated).unwrap());
            assert_eq!(builds.value(), 3);

            rebuild(&|| std::fs::write(&ready_path, b"onelf-view-ready 2\n").unwrap());
            assert_eq!(builds.value(), 4, "another layout's record is a miss");

            rebuild(&|| std::fs::write(&ready_path, b"onelf-view-ready 1\nand more\n").unwrap());
            assert_eq!(builds.value(), 5, "a record with additions is a miss");

            rebuild(&|| {
                std::fs::remove_file(&ready_path).unwrap();
                std::fs::create_dir(&ready_path).unwrap();
            });
            assert_eq!(
                builds.value(),
                6,
                "a directory in the record's place is a miss"
            );

            rebuild(&|| {
                std::fs::remove_file(&ready_path).unwrap();
                std::fs::write(view.join(CONFIGURATION_FILE), b"garbage").unwrap();
            });
            assert_eq!(
                builds.value(),
                7,
                "a damaged configuration record invalidates the view"
            );

            // A record of another identity, intact for that identity,
            // is not this one's.
            let other_root = OsStr::new("/onelf/fixtures/view");
            let other = ViewIdentity::new(&ViewInputs {
                uid: 1001,
                ..inputs(other_root, &sample_env(other_root), &sample_links())
            })
            .unwrap();
            rebuild(&|| {
                ViewRecord::write(&view, &other, None).unwrap();
                std::fs::write(&ready_path, &intact).unwrap();
            });
            assert_eq!(
                builds.value(),
                8,
                "a foreign identity's record never adopts the view"
            );

            let _ = std::fs::remove_dir_all(&dir);
        }

        /// A damaged view another launch still leases is never removed
        /// under it: the creator fails clearly, keeps its own shared
        /// lease, and leaves the tree whole. Once the other lease is gone
        /// the damage is rebuilt.
        #[test]
        fn a_damaged_view_is_not_replaced_while_another_launch_leases_it() {
            use rustix::fs::FlockOperation;
            let dir = temp_dir("leased-damage");
            let store = dir.join("store");
            let identity = fixture_identity();
            let view = view_path(&store, &identity);
            let builds = BuildCount::default();
            ensure_view(
                &store,
                &identity,
                &lease(&store, &identity),
                builds.build("first", &dir.join("journal")),
            )
            .expect("the first publication");
            std::fs::remove_file(view.join(READY_FILE)).unwrap();

            let other = lease(&store, &identity);
            let mine = lease(&store, &identity);
            let refused = ensure_view(
                &store,
                &identity,
                &mine,
                builds.build("again", &dir.join("journal")),
            );
            assert_eq!(
                refused.err().map(|e| e.kind()),
                Some(io::ErrorKind::ResourceBusy),
                "a leased damaged view is refused"
            );
            assert_eq!(builds.value(), 1, "nothing was rebuilt");
            assert!(
                view.join("bin/app").is_file(),
                "the leased tree stays whole"
            );
            let probe = std::fs::File::open(lease_lock_path(&view).unwrap()).unwrap();
            drop(other);
            assert!(
                rustix::fs::flock(&probe, FlockOperation::NonBlockingLockExclusive).is_err(),
                "the refused creator still holds its shared lease"
            );
            drop(probe);

            let rebuilt = ensure_view(
                &store,
                &identity,
                &mine,
                builds.build("again", &dir.join("journal")),
            )
            .expect("the rebuild once the other lease is gone");
            assert_view_complete(rebuilt.dir(), &identity, "again");
            assert_eq!(builds.value(), 2);

            let _ = std::fs::remove_dir_all(&dir);
        }

        /// The environment input a view carries must be exactly the one
        /// its record hashes: a changed or missing input is damage that
        /// rebuilds, never a view that is reused.
        #[test]
        fn an_environment_input_that_misses_its_hash_is_damage() {
            let dir = temp_dir("env-damage");
            let store = dir.join("store");
            let identity = fixture_identity();
            let view = view_path(&store, &identity);
            let env_path = view.join(crate::env::VIEW_ENV_FILE);
            let builds = BuildCount::default();
            let publish = |tag: &str| {
                ensure_view(
                    &store,
                    &identity,
                    &lease(&store, &identity),
                    builds.build(tag, &dir.join("journal")),
                )
                .expect("a publication")
            };

            publish("first");
            assert_eq!(std::fs::read(&env_path).unwrap(), fixture_env());
            publish("first");
            assert_eq!(builds.value(), 1, "an intact input is reused");

            std::fs::write(&env_path, b"FIXTURE_ENV=2\n").unwrap();
            let rebuilt = publish("changed");
            assert_eq!(builds.value(), 2, "a changed input is damage");
            assert_view_complete(rebuilt.dir(), &identity, "changed");
            assert_eq!(std::fs::read(&env_path).unwrap(), fixture_env());

            std::fs::remove_file(&env_path).unwrap();
            publish("missing");
            assert_eq!(builds.value(), 3, "a missing input is damage");

            let _ = std::fs::remove_dir_all(&dir);
        }

        /// Staging is scratch by name, never by content: even a dead
        /// creator's tree carrying a valid ready and configuration
        /// record is discarded, and a reader resolving by identity can
        /// only land on the view's own name.
        #[test]
        fn staging_is_never_adopted_as_a_view() {
            let dir = temp_dir("staging-names");
            let store = dir.join("store");
            let identity = fixture_identity();
            let view = view_path(&store, &identity);

            // The identity's name refuses any staging shape.
            assert_eq!(
                dir_name(&format!("{}{}", identity.key(), STAGING_SEP)),
                None,
                "a staging name is never a view name"
            );

            // An impostor's staging tree, fully dressed as a published
            // view, is still scratch.
            let impostor = store.join(format!("view-{}{}404-0", identity.key(), STAGING_SEP));
            std::fs::create_dir_all(impostor.join("bin")).unwrap();
            ViewRecord::write(&impostor, &identity, None).unwrap();
            std::fs::write(impostor.join(READY_FILE), ready_bytes()).unwrap();
            std::fs::write(impostor.join("bin/app"), b"app impostor\n").unwrap();

            let builds = BuildCount::default();
            let published = ensure_view(
                &store,
                &identity,
                &lease(&store, &identity),
                builds.build("fresh", &dir.join("journal")),
            )
            .expect("the fresh publication");
            assert_eq!(builds.value(), 1, "staging is never adopted");
            assert_eq!(published.dir(), view);
            assert_view_complete(published.dir(), &identity, "fresh");
            assert_eq!(
                std::fs::read(view.join("bin/app")).unwrap(),
                b"app fresh\n".to_vec(),
                "the impostor's content is gone"
            );
            assert!(staging_entries(&store, &identity).is_empty());

            let _ = std::fs::remove_dir_all(&dir);
        }

        /// Once published, a view's links never change: `place`
        /// refuses the tree outright, so neither an added, a
        /// retargeted, nor a removed link reaches a live view.
        #[test]
        fn published_links_never_change() {
            let dir = temp_dir("links-frozen");
            let store = dir.join("store");
            let identity = fixture_identity();
            let published = ensure_view(
                &store,
                &identity,
                &lease(&store, &identity),
                BuildCount::default().build("first", &dir.join("journal")),
            )
            .expect("the view");
            let manifest = empty_manifest();
            assert_eq!(
                present(&manifest, published.dir(), "lib"),
                sample_links(),
                "the published links are the staged ones"
            );

            let mut hostile = sample_links();
            hostile.insert(
                "libevil.so.1".to_string(),
                PathBuf::from("/host/lib/libevil.so.1"),
            );
            place(&manifest, published.dir(), "lib", &hostile);
            assert_eq!(
                present(&manifest, published.dir(), "lib"),
                sample_links(),
                "an added link never reaches a published view"
            );

            let mut retargeted = sample_links();
            *retargeted.get_mut("libfoo.so.1").unwrap() = PathBuf::from("/elsewhere/libfoo.so.1");
            place(&manifest, published.dir(), "lib", &retargeted);
            assert_eq!(
                present(&manifest, published.dir(), "lib"),
                sample_links(),
                "a retargeted link never reaches a published view"
            );

            place(&manifest, published.dir(), "lib", &Links::new());
            assert_eq!(
                present(&manifest, published.dir(), "lib"),
                sample_links(),
                "a removed link never reaches a published view"
            );
            assert!(ready(published.dir()), "the refusals leave the view intact");
            assert!(
                ViewRecord::load(published.dir(), &identity).is_some(),
                "the refusals leave the record intact"
            );

            let _ = std::fs::remove_dir_all(&dir);
        }

        /// Store content hardlinked into a view is never written
        /// through: the in-place gate refuses it, an edit is renamed
        /// onto a fresh inode, and the store's bytes stay intact under
        /// every other name that links them.
        #[test]
        fn a_store_hardlink_is_never_patched_in_place() {
            use std::os::unix::fs::MetadataExt;

            let dir = temp_dir("hardlink");
            let store = dir.join("store");
            let cas = dir.join("cas/ab/blob");
            std::fs::create_dir_all(cas.parent().unwrap()).unwrap();
            std::fs::write(&cas, b"store-bytes-v1").unwrap();
            // A second name over the same inode, as a package tree
            // would hold while the store keeps its own.
            let pkg = dir.join("pkg/app.so");
            std::fs::create_dir_all(pkg.parent().unwrap()).unwrap();
            std::fs::hard_link(&cas, &pkg).unwrap();
            let identity = fixture_identity();
            let builds = BuildCount::default();

            let published = ensure_view(
                &store,
                &identity,
                &lease(&store, &identity),
                |staged: &Staging| {
                    staged.hardlink(&cas, "lib/libf.so.1")?;
                    let linked = staged.dir().join("lib/libf.so.1");
                    assert_eq!(
                        std::fs::symlink_metadata(&linked).unwrap().nlink(),
                        3,
                        "the staged file shares the store's inode"
                    );

                    // A second name over the same inode, still unpatched.
                    staged.hardlink(&cas, "lib/raw.so.1")?;
                    assert_eq!(
                        std::fs::symlink_metadata(staged.dir().join("lib/raw.so.1"))
                            .unwrap()
                            .nlink(),
                        4,
                        "both staged names share the store's inode"
                    );

                    // The sanctioned change is a rename over the name: the
                    // name gets a fresh inode, the store keeps its bytes.
                    staged.edit_file("lib/libf.so.1", |bytes| {
                        let mut edited = bytes.to_vec();
                        edited.extend_from_slice(b"-patched");
                        Ok(edited)
                    })?;
                    assert_eq!(
                        std::fs::symlink_metadata(staged.dir().join("lib/libf.so.1"))
                            .unwrap()
                            .nlink(),
                        1,
                        "the edit gave the name a fresh inode"
                    );
                    assert_eq!(
                        std::fs::read(staged.dir().join("lib/libf.so.1")).unwrap(),
                        b"store-bytes-v1-patched".to_vec()
                    );
                    assert_eq!(
                        std::fs::read(&cas).unwrap(),
                        b"store-bytes-v1".to_vec(),
                        "the store's bytes are untouched by the edit"
                    );

                    builds.build("first", &dir.join("journal"))(staged)
                },
            )
            .expect("the view");

            // After publication the patched name is the view's alone,
            // the unpatched hardlink still shares the store's inode,
            // and the store's bytes are what they always were.
            let patched = published.dir().join("lib/libf.so.1");
            assert_eq!(
                std::fs::read(&patched).unwrap(),
                b"store-bytes-v1-patched".to_vec()
            );
            assert_eq!(std::fs::symlink_metadata(&patched).unwrap().nlink(), 1);
            let raw = published.dir().join("lib/raw.so.1");
            assert_eq!(std::fs::symlink_metadata(&raw).unwrap().nlink(), 3);
            assert_eq!(std::fs::read(&raw).unwrap(), b"store-bytes-v1".to_vec());
            assert_eq!(
                std::fs::read(&cas).unwrap(),
                b"store-bytes-v1".to_vec(),
                "the store blob is unchanged"
            );
            assert_eq!(
                std::fs::symlink_metadata(&cas).unwrap().nlink(),
                3,
                "the store keeps its links"
            );
            assert_eq!(std::fs::read(&pkg).unwrap(), b"store-bytes-v1".to_vec());

            let _ = std::fs::remove_dir_all(&dir);
        }

        /// The two lock classes compose in either order: a creator
        /// holding the store's exclusive lock can still build and reuse
        /// views, and a creator holding the view's creation mutex never
        /// waits on the store's lock, so no path can cycle.
        #[test]
        fn view_creation_and_the_store_lock_never_deadlock() {
            use rustix::fs::FlockOperation;
            let _locking = crate::test_locks::locking();
            let dir = temp_dir("locks");
            let store = dir.join("store");
            let identity = fixture_identity();
            let cas = dir.join("cas/ab/blob");
            std::fs::create_dir_all(cas.parent().unwrap()).unwrap();
            std::fs::write(&cas, b"store-bytes-v1").unwrap();
            let lock_store = |op: FlockOperation| {
                let f = std::fs::File::create(dir.join("cas.lock")).unwrap();
                rustix::fs::flock(&f, op).unwrap();
                f
            };

            // Canonical order: the store's exclusive lock, as a
            // publication or collection would hold it, then the view's
            // creation mutex, build and reuse included.
            {
                let held = lock_store(FlockOperation::LockExclusive);
                let published = ensure_view(
                    &store,
                    &identity,
                    &lease(&store, &identity),
                    |staged: &Staging| fixture_build(staged, "first", &dir.join("journal")),
                )
                .expect("the view builds under the store's exclusive lock");
                assert_view_complete(published.dir(), &identity, "first");
                let reused =
                    ensure_view(&store, &identity, &lease(&store, &identity), |_| Ok(None))
                        .expect("the view is reused under the store's exclusive lock");
                assert_eq!(reused.dir(), published.dir());
                drop(held);
            }

            // The other order: another holder keeps the store's
            // exclusive lock while this creator holds the creation
            // mutex and hardlinks store content. The build takes no
            // store lock, so it finishes without waiting.
            let other_root = OsStr::new("/onelf/fixtures/view");
            let other = ViewIdentity::new(&ViewInputs {
                uid: 1001,
                ..inputs(other_root, &sample_env(other_root), &sample_links())
            })
            .unwrap();
            let holder = dir.join("cas.lock");
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    let held = std::fs::File::create(&holder).unwrap();
                    rustix::fs::flock(&held, FlockOperation::LockExclusive).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    drop(held);
                });
                std::thread::sleep(std::time::Duration::from_millis(50));
                let published = ensure_view(
                    &store,
                    &other,
                    &lease(&store, &other),
                    |staged: &Staging| {
                        staged.hardlink(&cas, "lib/blob.so")?;
                        Ok(None)
                    },
                )
                .expect("the view builds without taking the store's lock");
                assert!(ready(published.dir()));
                assert_eq!(
                    std::fs::read(published.dir().join("lib/blob.so")).unwrap(),
                    b"store-bytes-v1".to_vec()
                );
            });

            let _ = std::fs::remove_dir_all(&dir);
        }

        /// A staging write refuses any path that is not a bounded
        /// relative path: absolute, traversal, interior NUL, past the
        /// depth bound. Nothing lands outside the store, and a source
        /// that is not a regular file is refused before a link is made.
        #[test]
        fn staging_writes_refuse_escaping_paths() {
            let dir = temp_dir("escaped");
            let store = dir.join("store");
            let identity = fixture_identity();
            let builds = BuildCount::default();

            let published = ensure_view(
                &store,
                &identity,
                &lease(&store, &identity),
                |staged: &Staging| {
                    for rel in ["", "..", "../escape", "a/../b", "/abs", "a\0b", "a/./b"] {
                        assert!(
                            staged.write_file(rel, b"x", 0o644).is_err(),
                            "{rel:?} must be refused"
                        );
                    }
                    let deep = (0..MAX_REL_DEPTH + 1)
                        .map(|i| format!("d{i}"))
                        .collect::<Vec<_>>()
                        .join("/");
                    assert!(
                        staged.write_file(&deep, b"x", 0o644).is_err(),
                        "a path past the depth bound is refused"
                    );
                    assert!(
                        staged.hardlink(&dir, "lib/dir").is_err(),
                        "a non-regular source is refused"
                    );
                    assert!(
                        staged.symlink("lib/up.so", "../../outside").is_err(),
                        "a target that walks out is refused"
                    );
                    assert!(
                        staged.symlink("lib/abs.so", "/etc/passwd").is_err(),
                        "an absolute target is refused"
                    );
                    assert!(
                        staged
                            .place_links(&empty_manifest(), "../lib", &Links::new())
                            .is_err(),
                        "a hostile link directory is refused"
                    );
                    for outside in [
                        dir.join("escape"),
                        store.join("escape"),
                        store.join("b"),
                        dir.join("b"),
                    ] {
                        assert!(!outside.exists(), "{} must not exist", outside.display());
                    }
                    builds.build("first", &dir.join("journal"))(staged)
                },
            )
            .expect("the valid build publishes");
            assert_view_complete(published.dir(), &identity, "first");
            assert!(
                !published.dir().join("lib/dir").exists(),
                "the refused hardlink left nothing staged"
            );

            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn a_staged_view_keeps_its_own_links_and_takes_the_farm_links() {
            let work = temp_dir("devfreelinks");
            let view = work.join("view-x");
            let staging = Staging::new(&view).unwrap();
            std::fs::create_dir_all(staging.dir().join("lib")).unwrap();
            std::fs::write(staging.dir().join("lib/libreal.so.1.0"), "elf").unwrap();
            std::os::unix::fs::symlink("libreal.so.1.0", staging.dir().join("lib/libreal.so.1"))
                .unwrap();

            let mut links = Links::new();
            links.insert(
                "libfarm.so".to_string(),
                PathBuf::from("/farm/libfarm.so.1"),
            );
            staging.place_free_links("lib", &links).unwrap();

            assert_eq!(
                std::fs::read_link(staging.dir().join("lib/libreal.so.1")).unwrap(),
                PathBuf::from("libreal.so.1.0")
            );
            assert_eq!(
                std::fs::read_link(staging.dir().join("lib/libfarm.so")).unwrap(),
                PathBuf::from("/farm/libfarm.so.1")
            );

            let _ = std::fs::remove_dir_all(&work);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runpath_entry_is_folded_against_the_files_directory() {
        assert_eq!(
            normalize(Path::new("/mnt/pkg/bin/../lib")),
            PathBuf::from("/mnt/pkg/lib")
        );
        assert_eq!(
            normalize(Path::new("/mnt/pkg/opt/x/bin/../../../lib/./sub")),
            PathBuf::from("/mnt/pkg/lib/sub")
        );
    }

    #[test]
    fn a_dev_link_dir_prefers_the_shallowest_runpath_directory_inside_the_tree() {
        let root = Path::new("/pkg");
        let exists = |d: &str| matches!(d, "lib" | "deep/a/b" | "opt/lib");
        assert_eq!(
            dir_where(
                root,
                &[root.join("deep/a/b"), root.join("lib")],
                &["lib"],
                exists
            ),
            Some("lib".to_string())
        );
        assert_eq!(
            dir_where(root, &[root.join("deep/a/b")], &["lib", "opt/lib"], exists),
            Some("deep/a/b".to_string())
        );
        assert_eq!(
            dir_where(root, &[root.join("ghost")], &["missing", "opt/lib"], exists),
            Some("opt/lib".to_string())
        );
        assert_eq!(
            dir_where(
                root,
                &[PathBuf::from("/elsewhere/lib")],
                &["missing"],
                exists
            ),
            None
        );
    }

    #[test]
    fn a_dev_tree_wants_what_it_does_not_carry() {
        let work = std::env::temp_dir().join(format!("onelf-devwanted-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        let farm = work.join("farm");
        let set = work.join("set");
        std::fs::create_dir_all(&farm).unwrap();
        std::fs::create_dir_all(set.join("lib")).unwrap();
        std::os::unix::fs::symlink("/real/libfarm.so.1", farm.join("libfarm.so")).unwrap();
        std::fs::write(set.join("lib/libset.so.1"), "elf").unwrap();

        let links = wanted_from(Some(&farm), std::slice::from_ref(&set), &HashSet::new());
        assert_eq!(
            links.get("libfarm.so"),
            Some(&PathBuf::from("/real/libfarm.so.1"))
        );
        assert_eq!(links.get("libset.so.1"), Some(&set.join("lib/libset.so.1")));

        let carried = HashSet::from(["libfarm.so".to_string()]);
        let links = wanted_from(Some(&farm), std::slice::from_ref(&set), &carried);
        assert!(!links.contains_key("libfarm.so"));
        assert!(links.contains_key("libset.so.1"));

        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn links_survive_the_trip_to_the_server() {
        let (tx, mut rx) = channel().unwrap();
        let mut links = Links::new();
        links.insert("libGL.so.1".into(), PathBuf::from("/usr/lib/libGL.so.1"));
        links.insert(
            "libfixture.so.1".into(),
            PathBuf::from("/run/set/lib/libfixture.so.1"),
        );
        let mut shadows = Links::new();
        shadows.insert("lib/libc.so.6".into(), PathBuf::from("/usr/lib/libc.so.6"));
        let sender = std::thread::spawn(move || {
            tx.send("lib", &links, &shadows, b"ONELF-RUNTIME-ENV 1 0\n")
        });
        let got = loop {
            if let Received::Done(got) = rx.read() {
                break got;
            }
        };
        rx.ack();
        sender.join().unwrap();
        let got = got.expect("a message");
        assert_eq!(got.dir, "lib");
        assert_eq!(got.env, b"ONELF-RUNTIME-ENV 1 0\n");
        assert_eq!(
            got.links,
            vec![
                ("libGL.so.1".to_string(), b"/usr/lib/libGL.so.1".to_vec()),
                (
                    "libfixture.so.1".to_string(),
                    b"/run/set/lib/libfixture.so.1".to_vec()
                ),
            ]
        );
        assert_eq!(
            got.shadows,
            vec![("lib/libc.so.6".to_string(), b"/usr/lib/libc.so.6".to_vec())]
        );
    }
}

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
pub fn dir_index(manifest: &Manifest, rel: &str) -> Option<usize> {
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
pub fn dir(
    manifest: &Manifest,
    pkg_root: &Path,
    runpath: &[PathBuf],
    lib_dirs: &[&str],
) -> Option<String> {
    let exists = |d: &&str| !d.is_empty() && dir_index(manifest, d).is_some();
    runpath
        .iter()
        .filter_map(|d| d.strip_prefix(pkg_root).ok()?.to_str())
        .filter(exists)
        .min_by_key(|d| Path::new(d).components().count())
        .or_else(|| lib_dirs.iter().copied().find(exists))
        .map(String::from)
}

/// The names the package itself has in `dir`.
fn carried<'m>(manifest: &'m Manifest, dir: &str) -> HashSet<&'m str> {
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
pub fn wanted(manifest: &Manifest, dir: &str, farm: Option<&Path>, set_roots: &[PathBuf]) -> Links {
    let carried = carried(manifest, dir);
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
fn present(manifest: &Manifest, root: &Path, dir: &str) -> Links {
    let carried = carried(manifest, dir);
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
pub fn place(manifest: &Manifest, root: &Path, dir: &str, links: &Links) {
    let base = root.join(dir);
    let present = present(manifest, root, dir);
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
pub struct Receiver {
    rx: OwnedFd,
    ack: OwnedFd,
    buf: Vec<u8>,
}

/// What the server has heard from the child so far.
pub enum Received {
    /// More is on its way.
    Pending,
    /// The child is done: its links, or nothing when it went away or sent
    /// something unreadable.
    Done(Option<Message>),
}

/// The links for one directory, as the server is sent them.
pub struct Message {
    /// The directory, relative to the package root.
    pub dir: String,
    /// Each link's name and target.
    pub links: Vec<(String, Vec<u8>)>,
}

/// A pipe pair for handing the links from the child that resolves them to
/// the server that serves them. Made before the fork; the child keeps the
/// [`Sender`] and the server the [`Receiver`]. Every descriptor is
/// close-on-exec, so the app inherits none of them.
pub fn channel() -> io::Result<(Sender, Receiver)> {
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
    /// Send `links` for `dir` and wait until the server serves them. A
    /// server that has gone away ends the wait as well.
    pub fn send(&self, dir: &str, links: &Links) {
        let mut body = Vec::new();
        body.extend_from_slice(dir.as_bytes());
        body.push(0);
        for (name, target) in links {
            body.extend_from_slice(name.as_bytes());
            body.push(0);
            body.extend_from_slice(target.as_os_str().as_bytes());
            body.push(0);
        }
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
    pub fn fd(&self) -> &OwnedFd {
        &self.rx
    }

    /// Take what the pipe holds. Call when [`Receiver::fd`] polls readable.
    pub fn read(&mut self) -> Received {
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
    pub fn ack(&self) {
        let _ = rustix::io::write(&self.ack, b"k");
    }
}

fn decode(body: &[u8]) -> Option<Message> {
    let mut fields = body.split(|&b| b == 0);
    let dir = std::str::from_utf8(fields.next()?).ok()?.to_string();
    let mut links = Vec::new();
    while let Some(name) = fields.next().filter(|f| !f.is_empty()) {
        let target = fields.next()?;
        links.push((std::str::from_utf8(name).ok()?.to_string(), target.to_vec()));
    }
    Some(Message { dir, links })
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
pub(crate) mod view {
    use std::ffi::OsStr;
    use std::fmt;
    use std::io::{self, Write};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use onelf_format::HostLibsPolicy;
    use onelf_format::resolve::{ActivePin, SelectionIdentity};

    use super::Links;

    /// The view-layout version every identity names. A bump here
    /// separates every view an older runtime built from the ones the
    /// newer layout builds, and makes any record naming another version
    /// a miss rather than a misread.
    pub const LAYOUT: u32 = 1;

    /// The name of the configuration record inside a view directory.
    pub const CONFIGURATION_FILE: &str = "config";

    /// How the launch's library links (see [`crate::links`]) reach the
    /// view.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[allow(dead_code)]
    pub enum Placement {
        /// The tree is this launch's own and on disk: the links are
        /// placed in it directly.
        Tree,
        /// The mount is this launch's own, served by the server it
        /// started.
        Mount,
        /// The mount is another launch's, which must already serve the
        /// links this launch needs.
        Served,
    }

    impl Placement {
        fn as_str(self) -> &'static str {
            match self {
                Placement::Tree => "tree",
                Placement::Mount => "mount",
                Placement::Served => "served",
            }
        }
    }

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
    #[allow(dead_code)]
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
        /// How the library links reach the view.
        pub placement: Placement,
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
    #[allow(dead_code)]
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
        #[allow(dead_code)]
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
        #[allow(dead_code)]
        pub fn key(&self) -> &str {
            &self.key
        }

        /// The canonical text the identity is, as built. This is the
        /// exact text a configuration record stores and binds itself to.
        #[allow(dead_code)]
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
    #[allow(dead_code)]
    pub struct ViewRecord {
        canonical: Vec<u8>,
        expanded_env: Option<String>,
    }

    impl ViewRecord {
        /// Write the record for `identity` into `dir`, replacing any
        /// earlier one atomically. `expanded_env` is the environment
        /// input as finally expanded into the view, hashed here so no
        /// value is stored or echoed; `None` until the view is built.
        #[allow(dead_code)]
        pub fn write(
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
        #[allow(dead_code)]
        pub fn load(dir: &Path, identity: &ViewIdentity) -> Option<Self> {
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

        /// The canonical configuration the record was written from.
        #[allow(dead_code)]
        pub fn configuration(&self) -> &[u8] {
            &self.canonical
        }

        /// The content hash of the environment input as finally expanded
        /// into the view, or `None` while the view is still being built.
        #[allow(dead_code)]
        pub fn expanded_env(&self) -> Option<&str> {
            self.expanded_env.as_deref()
        }
    }

    /// The view directory's name for an identity key, `view-<key>`, or
    /// `None` when `key` is not a key [`ViewIdentity`] mints: the name
    /// is one bounded lowercase hex component, so nothing a caller
    /// passes can name anything outside the store.
    #[allow(dead_code)]
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
        c.extend_from_slice(format!("place {}\n", inputs.placement.as_str()).as_bytes());
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
            &b"place "[..],
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
        use std::path::PathBuf;

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
                placement: Placement::Tree,
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
                    "placement",
                    ViewInputs {
                        placement: Placement::Mount,
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
            assert_eq!(record.configuration(), identity.canonical());
            assert_eq!(
                record.expanded_env(),
                Some(blake3::hash(&env).to_hex().to_string()).as_deref()
            );

            // The configuration answers what was built without
            // recomputing: every input is on the record.
            let text = String::from_utf8_lossy(record.configuration());
            assert!(text.contains("uid 1000"));
            assert!(text.contains("policy auto"));
            assert!(text.contains("place tree"));
            assert!(text.contains("entry 3:app"));
            assert!(text.contains("libfoo.so.1"));

            // A rewrite replaces the record whole, and an unbuilt view
            // records no expanded hash.
            ViewRecord::write(&dir, &identity, None).unwrap();
            assert_eq!(names(&dir), [CONFIGURATION_FILE.to_string()]);
            assert_eq!(
                ViewRecord::load(&dir, &identity).unwrap().expanded_env(),
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
            miss(&swap(&good, b"place tree", b"place mounx"));
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
    fn links_survive_the_trip_to_the_server() {
        let (tx, mut rx) = channel().unwrap();
        let mut links = Links::new();
        links.insert("libGL.so.1".into(), PathBuf::from("/usr/lib/libGL.so.1"));
        links.insert(
            "libfixture.so.1".into(),
            PathBuf::from("/run/set/lib/libfixture.so.1"),
        );
        let sender = std::thread::spawn(move || tx.send("lib", &links));
        let got = loop {
            if let Received::Done(got) = rx.read() {
                break got;
            }
        };
        rx.ack();
        sender.join().unwrap();
        let got = got.expect("a message");
        assert_eq!(got.dir, "lib");
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
    }
}

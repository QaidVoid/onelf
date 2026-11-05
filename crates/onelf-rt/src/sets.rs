//! Dependency sets a package shares with others instead of bundling.
//!
//! A set is the closure of some sysroot packages, packed as a build and
//! pinned by hash in `.onelf/sets`. The bundle carries none of it, so a
//! set is not conditional the way the GL build is: every set the package
//! names has to be there, and a set that cannot be obtained is a failed
//! launch rather than a degraded one.
//!
//! Obtaining one is the GL build's path exactly, so two packages naming
//! the same set share one download.
//!
//! Which sets a launch obtains is the invoked entrypoint's selection. A
//! package that carries no selection record keeps the legacy behavior:
//! every set the table declares. A carried record scopes acquisition to
//! the invoked entrypoint's list, so an entrypoint selecting nothing
//! leaves every toolkit artifact untouched and one naming a few obtains
//! exactly those pins; a record the set table cannot back ends the
//! launch before anything is acquired. The optional pinned GL build
//! keeps its own warn-and-continue policy in the launch, and a required
//! set's failure is never folded into it. Each obtained set's objects
//! are then checked against what the launch can satisfy, in
//! [`crate::providers`], so an old artifact whose objects cannot run
//! beside this package ends the launch before the application does.
//!
//! What they do with it depends on the mode. Mounted, the set is served
//! from its stored file by a detached FUSE server at a mountpoint named
//! after the build's hash, so nothing is copied to disk and every package
//! naming that set shares one mount. The server lives in the host mount
//! namespace, which is why sets are obtained before any execution mode
//! makes a namespace of its own: a mount made afterwards would be private
//! to one launch. Extracted, the set goes through the ordinary package
//! cache, which costs a full copy but needs no FUSE.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use onelf_format::Manifest;
use onelf_format::resolve::ActivePin;
use onelf_format::rules::RuleId;
use onelf_format::runtime_metadata::{EntrypointSets, SetPin};

use crate::platform::Pin;

/// Where the package records the sets it needs, relative to the package
/// root.
pub const SETS_FILE: &str = ".onelf/sets";

/// One obtained set: the extracted root and the lock that keeps the
/// extraction alive for this instance.
pub(crate) struct Obtained {
    /// The set table's declaration the build was obtained for.
    pub label: String,
    /// The BLAKE3 digest the build is pinned by, in lowercase hex.
    pub blake3: String,
    /// The extracted or mounted root of the set's build.
    pub root: PathBuf,
    pub _lock: fs::File,
}

/// The sets the record names, in file order. A malformed table is
/// skipped rather than guessed at; the launch then fails on the missing
/// libraries, which names the problem better than a parse error would.
pub fn parse(text: &str) -> Vec<Pin> {
    let mut out: Vec<Pin> = Vec::new();
    let mut name: Option<String> = None;
    let (mut url, mut blake3) = (None, None);
    let flush = |name: &mut Option<String>,
                 url: &mut Option<String>,
                 blake3: &mut Option<String>,
                 out: &mut Vec<Pin>| {
        if let (Some(label), Some(url), Some(blake3)) = (name.take(), url.take(), blake3.take())
            && let Some(pin) = crate::platform::checked_pin(label, url, blake3)
        {
            out.push(pin);
        }
    };
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix('[') {
            flush(&mut name, &mut url, &mut blake3, &mut out);
            name = rest.strip_suffix(']').map(|n| n.trim().to_string());
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        let Some(value) = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .map(|v| v.replace("\\\"", "\"").replace("\\\\", "\\"))
        else {
            continue;
        };
        match key.trim() {
            "url" => url = Some(value),
            "blake3" => blake3 = Some(value.to_ascii_lowercase()),
            _ => {}
        }
    }
    flush(&mut name, &mut url, &mut blake3, &mut out);
    out
}

/// The pins the invoked entrypoint acquires, resolved from the set
/// table `table` and the decoded selection record `selections`.
///
/// No record is the legacy launch: every set the table declares, in
/// table order. A record is checked against the manifest's entrypoint
/// names and the table's actual pins first, so a record the package
/// cannot back is refused before anything is acquired. The entrypoint's
/// own list then names exactly the pins to obtain, in the record's
/// canonical name order; an empty list names none.
pub fn selection(
    table: Option<&[u8]>,
    selections: Option<&EntrypointSets>,
    manifest: &Manifest,
    entrypoint: &str,
) -> Result<Vec<Pin>, String> {
    let pins = table
        .map(|bytes| parse(&String::from_utf8_lossy(bytes)))
        .unwrap_or_default();
    let Some(selections) = selections else {
        return Ok(pins);
    };
    let backed = pins
        .iter()
        .map(declared_pin)
        .collect::<Result<Vec<SetPin>, String>>()?;
    let names: Vec<&str> = manifest
        .entrypoints
        .iter()
        .map(|entrypoint| manifest.get_string(entrypoint.name))
        .collect();
    selections
        .validate_against(&names, &backed)
        .map_err(|why| format!("entrypoint-sets: {why}"))?;
    let Some(selected) = selections
        .entrypoints
        .iter()
        .find(|entry| entry.name == entrypoint)
    else {
        return Err(format!(
            "entrypoint-sets: no selection for entrypoint {entrypoint}"
        ));
    };
    selected
        .sets
        .iter()
        .map(|name| {
            pins.iter()
                .find(|pin| pin.label == name.as_str())
                .map(|pin| Pin {
                    label: pin.label.clone(),
                    url: pin.url.clone(),
                    blake3: pin.blake3.clone(),
                })
                .ok_or_else(|| {
                    format!(
                        "entrypoint-sets: the set table does not declare set {}",
                        name.as_str()
                    )
                })
        })
        .collect()
}

/// Every declaration of the set table `table` in the selection record's
/// form, sorted by name, the pins a runtime rule binding may name when no
/// selection record carries them. A missing table declares none.
pub fn declared_pins(table: Option<&[u8]>) -> Result<Vec<SetPin>, String> {
    let mut pins = table
        .map(|bytes| parse(&String::from_utf8_lossy(bytes)))
        .unwrap_or_default()
        .iter()
        .map(declared_pin)
        .collect::<Result<Vec<SetPin>, String>>()?;
    pins.sort();
    Ok(pins)
}

/// The selection record's form of a set table declaration: the table's
/// name as a canonical set identity and the digest as full bytes, the
/// exact pairing [`EntrypointSets::validate_against`] checks.
fn declared_pin(pin: &Pin) -> Result<SetPin, String> {
    let name = RuleId::new(&pin.label).map_err(|_| {
        format!(
            "entrypoint-sets: the set table's name '{}' is not a canonical set name",
            pin.label
        )
    })?;
    Ok(SetPin {
        name,
        hash: hex_digest(&pin.blake3).ok_or_else(|| {
            format!(
                "entrypoint-sets: the set table's pin for '{}' is not one BLAKE3 digest",
                pin.label
            )
        })?,
    })
}

/// The 32 bytes of a hex BLAKE3 digest, or `None` for anything else.
fn hex_digest(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let bytes = text.as_bytes();
    let mut digest = [0u8; 32];
    for (byte, pair) in digest.iter_mut().zip(bytes.as_chunks::<2>().0) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        *byte = ((hi << 4) | lo) as u8;
    }
    Some(digest)
}

/// The sets this launch obtained, held for the life of the process: the
/// locks in them are what keep a mount or an extraction from being
/// reclaimed while the application runs, and they are inheritable so the
/// claim survives the `exec`.
static OBTAINED: OnceLock<Vec<Obtained>> = OnceLock::new();

/// Obtain the selected entrypoint's sets, before any execution mode
/// makes a mount namespace of its own.
///
/// `pins` names exactly what to acquire: an empty selection acquires
/// nothing and leaves every toolkit artifact untouched. A set that
/// cannot be had ends the launch naming the entrypoint and the pin: the
/// bundle carries none of it, so there is nothing to degrade to, and
/// the failure is never folded into the optional GL build's separate
/// warn-and-continue policy. Each obtained set's objects are inspected
/// as they arrive, so a build whose ELF data is truncated or malformed
/// ends the launch here, before any further pin is acquired and before
/// a mode makes a namespace of its own; the facts stay with the launch
/// for its provider compatibility check.
pub fn init(pins: &[Pin], entrypoint: &str) {
    let mut out = Vec::new();
    let mut prepared = Vec::new();
    for pin in pins {
        match obtain(pin, &out) {
            Ok(obtained) => {
                match crate::providers::prepare(&pin.label, &pin.blake3, &obtained.root) {
                    Ok(facts) => {
                        prepared.push(facts);
                        out.push(obtained);
                    }
                    Err(why) => {
                        eprintln!(
                            "onelf-rt: dependency set {} required by entrypoint {entrypoint}: {why}",
                            pin.label
                        );
                        std::process::exit(1);
                    }
                }
            }
            Err(why) => {
                eprintln!(
                    "onelf-rt: dependency set {} required by entrypoint {entrypoint}: {why}",
                    pin.label
                );
                std::process::exit(1);
            }
        }
    }
    crate::providers::record_selected(prepared);
    let _ = OBTAINED.set(out);
}

/// The pins this launch obtained, in acquisition order: the selection a
/// view's identity covers. A launch that obtained none has none.
pub fn pins() -> Vec<ActivePin> {
    OBTAINED
        .get()
        .map(|sets| {
            sets.iter()
                .map(|set| ActivePin {
                    label: set.label.clone(),
                    hash: set.blake3.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The extracted or mounted root of every set this launch obtained, in
/// acquisition order. A caller that drives acquisition itself, as the
/// CLI's development run does, reads these to put what the sets supply
/// on its launch's own resolution path; a launch that acquired no set
/// has none.
pub fn roots() -> Vec<PathBuf> {
    OBTAINED
        .get()
        .map(|sets| sets.iter().map(|s| s.root.clone()).collect())
        .unwrap_or_default()
}

/// Whether the set should be served from its stored file rather than
/// copied out of it. `ONELF_SET_MODE` forces either way; mounting is the
/// default and falls back to extraction wherever FUSE cannot serve.
fn mount_wanted() -> bool {
    !matches!(std::env::var("ONELF_SET_MODE").as_deref(), Ok("extract"))
}

/// Obtain one set. `earlier` are the sets this launch already holds.
fn obtain(pin: &Pin, earlier: &[Obtained]) -> Result<Obtained, String> {
    let file = crate::platform::store_build(pin)?;
    if mount_wanted() {
        match mount(&file, pin, earlier) {
            Ok(obtained) => return Ok(obtained),
            Err(why) => eprintln!(
                "onelf-rt: dependency set {}: cannot mount ({why}); extracting instead",
                pin.label
            ),
        }
    }
    let (root, lock) = crate::platform::extract_build(&file)?;
    Ok(Obtained {
        label: pin.label.clone(),
        blake3: pin.blake3.clone(),
        root,
        _lock: lock,
    })
}

/// Serve the verified build `artifact` from a mountpoint named after the
/// pin's hash, or join the mount another instance already serves there.
/// The server loads the container through a duplicate of the verified
/// handle, never by reopening the stored name. `earlier` are the sets
/// this launch already holds.
fn mount(
    artifact: &crate::platform::VerifiedArtifact,
    pin: &Pin,
    earlier: &[Obtained],
) -> Result<Obtained, String> {
    use rustix::runtime::{Fork, kernel_fork};

    let (mountpoint, lock) =
        crate::paths::create_set_mountpoint(&pin.blake3).ok_or("no private runtime directory")?;

    let dir_name = mountpoint
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    let joined = |lock: fs::File| Obtained {
        label: pin.label.clone(),
        blake3: pin.blake3.clone(),
        root: mountpoint.clone(),
        _lock: lock,
    };

    // Another instance is already serving this set: its mount is in the
    // host namespace, so it is ours to use as well. The shared lock just
    // taken is what stops it being reclaimed under us.
    if served(&mountpoint) {
        return Ok(joined(lock));
    }
    // Only the helper will do. The other mount path unshares a mount
    // namespace, and a mount made there is invisible outside it: the
    // application is a different process, and a second package could
    // never join it. A namespace mount would also have to be made before
    // the mode is chosen, putting every launch with a set into a user
    // namespace, which is exactly what a `needs-setuid` package and a
    // rootless podman or distrobox host ask the runtime not to do.
    if !crate::fuse::mount::fusermount3_available() {
        return Err(
            "a shared mount needs fusermount3, which is not on PATH (install fuse3, \
             or set ONELF_SET_MODE=extract to stop asking)"
                .into(),
        );
    }

    // One launch mounts, the rest wait and join what it made.
    let _creating = crate::paths::lock_set_creation(&dir_name);
    if served(&mountpoint) {
        return Ok(joined(lock));
    }
    let file = artifact.handle()?;

    match unsafe { kernel_fork() } {
        Ok(Fork::Child(_)) => {
            // The inherited lock is the parent's claim, not the server's:
            // closing this descriptor leaves the parent's open file
            // description holding it, and lets the server ask whether
            // anyone still does. The creation lock goes too, or the server
            // would hold it for as long as it serves.
            drop(lock);
            drop(_creating);
            // The claims on sets obtained before this one are the
            // launch's as well. Held here, they would keep those sets'
            // servers up for as long as this one serves. The server never
            // returns, so nothing closes these descriptors a second time.
            for set in earlier {
                unsafe { rustix::io::close(std::os::fd::AsRawFd::as_raw_fd(&set._lock)) };
            }
            serve(file, &mountpoint);
        }
        Ok(Fork::ParentOf(child)) => {
            for _ in 0..400 {
                if served(&mountpoint) {
                    return Ok(joined(lock));
                }
                if matches!(
                    rustix::process::waitpid(Some(child), rustix::process::WaitOptions::NOHANG),
                    Ok(Some(_))
                ) {
                    return Err("the server exited before the mount appeared".into());
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err("the mount did not appear".into())
        }
        Err(e) => Err(format!("fork: {e}")),
    }
}

/// Whether something is already serving `mountpoint`: a mount is there
/// and answers a directory read, which a mount whose server died does
/// not.
fn served(mountpoint: &Path) -> bool {
    crate::paths::is_mountpoint(mountpoint) && mountpoint.read_dir().is_ok()
}

/// The detached server: mount the verified build `file` holds, serve
/// until nothing claims the mountpoint any more, then take it down.
/// Never returns.
fn serve(file: fs::File, mountpoint: &Path) -> ! {
    // Out of the launcher's session and off its standard streams, so a
    // Ctrl-C aimed at the application does not take the set down and
    // `app | head` does not wait on this process.
    let _ = rustix::process::setsid();
    if let Ok(null) = rustix::fs::open(
        "/dev/null",
        rustix::fs::OFlags::RDWR,
        rustix::fs::Mode::empty(),
    ) {
        for target in 0..=2 {
            use std::os::fd::FromRawFd;
            let mut slot =
                std::mem::ManuallyDrop::new(unsafe { std::os::fd::OwnedFd::from_raw_fd(target) });
            let _ = rustix::io::dup2(&null, &mut slot);
        }
    }

    let served = (|| -> Result<(), String> {
        let fuse_fd = crate::fuse::mount::fuse_mount(mountpoint).map_err(|e| e.to_string())?;
        let mut pkg = crate::loader::load_file(file).map_err(|e| e.to_string())?;
        let lock_path = crate::paths::set_lock_path(
            mountpoint
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default(),
        );
        let mut state =
            crate::fuse::fs::FuseState::new(&pkg.manifest, &mut pkg.file, &pkg.footer, &pkg.codec);
        // Opened now, while the file is certain to exist: if the runtime
        // directory is cleaned out from under the server, this is what
        // still says whether a launch holds the claim.
        let original = lock_path.as_deref().and_then(|p| fs::File::open(p).ok());
        let mut buf = vec![0u8; 1024 * 1024 + 4096];
        state.serve_detached(&fuse_fd, &mut buf, || {
            lock_path
                .as_deref()
                .is_none_or(|p| unclaimed(p, original.as_ref()))
        });
        drop(fuse_fd);
        Ok(())
    })();
    // A server that never mounted has nothing to take down, and
    // unmounting what is not mounted is a no-op, so one path serves both.
    let _ = served;
    crate::fuse::mount::fuse_unmount(mountpoint);
    let _ = std::fs::remove_dir(mountpoint);
    std::process::exit(0);
}

/// Whether nobody holds the mountpoint's shared lock any more, which is
/// what says the last package using this set is gone. Tested on a fresh
/// descriptor each time, since a lock belongs to the open file
/// description that took it and a later launch may have made the file
/// anew. A lock file that is gone is asked through `original`, the
/// descriptor the server opened at the start: the launches holding the
/// claim hold it on that file, so a cleaned-out runtime directory neither
/// keeps the server forever nor ends it under a running app.
fn unclaimed(lock_path: &Path, original: Option<&fs::File>) -> bool {
    let probe = |lock: &fs::File| {
        rustix::fs::flock(lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok()
    };
    match fs::File::open(lock_path) {
        Ok(lock) => probe(&lock),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => original.is_none_or(probe),
        Err(_) => false,
    }
}

/// Every directory in the set that holds a shared object, `lib` first.
///
/// A set keeps the layout the distribution used, so a library that keeps
/// its plugins in a directory of its own still has them there, and the
/// absolute run path it was built with named the host. Putting each such
/// directory on this launch's library path is what the packer does for a
/// bundle's own tree, and it is the only thing that reaches a plugin the
/// loader is asked for by bare soname. Public for the same caller
/// [`roots`](fn@roots) serves: a development launch resolving against
/// acquired sets needs the directories the resolver indexes.
pub fn lib_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = onelf_format::resolve::GL_BUILD_LIB_DIRS
        .iter()
        .map(|d| root.join(d))
        .filter(|d| d.is_dir())
        .collect();
    let mut queue: Vec<PathBuf> = dirs.clone();
    while let Some(dir) = queue.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let holds_objects = fs::read_dir(&path).is_ok_and(|mut e| {
                e.any(|f| f.is_ok_and(|f| f.file_name().to_string_lossy().contains(".so")))
            });
            if holds_objects && !dirs.contains(&path) {
                dirs.push(path.clone());
            }
            queue.push(path);
        }
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lock_file_cleaned_away_is_asked_through_the_original() {
        let _locking = crate::test_locks::locking();
        let dir = std::env::temp_dir().join(format!("onelf-unclaimed-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("set.lock");
        fs::write(&path, b"").unwrap();
        let original = fs::File::open(&path).unwrap();
        let claim = fs::File::open(&path).unwrap();
        rustix::fs::flock(&claim, rustix::fs::FlockOperation::LockShared).unwrap();

        fs::remove_dir_all(&dir).unwrap();
        assert!(
            !unclaimed(&path, Some(&original)),
            "a launch still holds the claim on the removed file"
        );
        drop(claim);
        assert!(
            unclaimed(&path, Some(&original)),
            "the last claim is gone, so the server may go"
        );
        assert!(
            unclaimed(&path, None),
            "nothing to ask means nobody claims it"
        );
    }

    #[test]
    fn a_record_yields_one_pin_per_table() {
        let hash = "ab".repeat(32);
        let text = format!(
            "[qt]\nurl = \"file:///a/qt.onelf\"\nblake3 = \"{hash}\"\npackages = [\"qt6-base\"]\n\n\
             [gtk]\nurl = \"https://e/gtk.onelf\"\nblake3 = \"{hash}\"\npackages = []\n"
        );
        let pins = parse(&text);
        assert_eq!(pins.len(), 2);
        assert_eq!(pins[0].label, "qt");
        assert_eq!(pins[0].url, "file:///a/qt.onelf");
        assert_eq!(pins[1].label, "gtk");
        assert_eq!(pins[1].blake3, hash);
    }

    #[test]
    fn a_table_missing_a_field_or_carrying_a_bad_one_is_skipped() {
        let hash = "cd".repeat(32);
        let text = format!(
            "[nourl]\nblake3 = \"{hash}\"\n\n\
             [badhash]\nurl = \"file:///a\"\nblake3 = \"abc\"\n\n\
             [badname/x]\nurl = \"file:///a\"\nblake3 = \"{hash}\"\n\n\
             [ok]\nurl = \"file:///a\"\nblake3 = \"{hash}\"\n"
        );
        let pins = parse(&text);
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].label, "ok");
    }
}

/// The selection a launch resolves from a package's records, and the
/// acquisition scoping it produces.
///
/// The pure cases run `selection` against crafted tables and records.
/// The end-to-end cases pack fixtures under the built runtime's prefix
/// and run them with the store hint, cache root and mode environment
/// pointed at the case's private directories, so which sets the launch
/// attempted, which it obtained, and which it left alone are observable
/// on disk and on stderr. Acquisition cannot be forced twice in one
/// process, so the launch side is observed from the child, never here.
#[cfg(test)]
mod selection_tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    use onelf_format::rules::RuleId;
    use onelf_format::runtime_metadata::{EntrypointSelection, EntrypointSets, SetPin};
    use onelf_format::{EntryPoint, EntryPointFlags, Manifest, ManifestHeader, WorkingDir};

    use super::selection;
    use crate::platform::Pin;

    const PIN_A: [u8; 32] = [0xaa; 32];
    const PIN_B: [u8; 32] = [0xbb; 32];

    /// A sequence number keeping every test's directories apart.
    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// The lowercase hex of a digest, the form a set table pins by.
    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The url a source file has.
    fn file_url(path: &Path) -> String {
        format!("file://{}", path.display())
    }

    /// A set table declaring `(label, url, hash)` triples.
    fn table(decls: &[(&str, &str, &str)]) -> Vec<u8> {
        let mut text = String::new();
        for (label, url, hash) in decls {
            text.push_str(&format!(
                "[{label}]\nurl = \"{url}\"\nblake3 = \"{hash}\"\n\n"
            ));
        }
        text.into_bytes()
    }

    /// A set table whose sources no fetch can satisfy.
    fn unreachable_table(decls: &[(&str, &str)]) -> Vec<u8> {
        let mut text = String::new();
        for (label, hash) in decls {
            text.push_str(&format!(
                "[{label}]\nurl = \"file:///nonexistent/{label}.onelf\"\nblake3 = \"{hash}\"\n\n"
            ));
        }
        text.into_bytes()
    }

    /// Encoded selection-record bytes for `pins` and per-entrypoint
    /// selections.
    fn record(pins: &[(&str, [u8; 32])], entries: &[(&str, &[&str])]) -> Vec<u8> {
        EntrypointSets {
            pins: pins
                .iter()
                .map(|(name, hash)| SetPin {
                    name: RuleId::new(name).unwrap(),
                    hash: *hash,
                })
                .collect(),
            entrypoints: entries
                .iter()
                .map(|(name, sets)| EntrypointSelection {
                    name: name.to_string(),
                    sets: sets.iter().map(|s| RuleId::new(s).unwrap()).collect(),
                })
                .collect(),
        }
        .encode()
        .unwrap()
    }

    /// The selection record `bytes` decodes to.
    fn decoded(bytes: &[u8]) -> EntrypointSets {
        EntrypointSets::decode(bytes).unwrap()
    }

    /// A manifest whose entrypoints are the named records.
    fn manifest(names: &[&str]) -> Manifest {
        let mut string_table: Vec<u8> = vec![0];
        let mut entrypoints = Vec::new();
        for name in names {
            let offset = string_table.len() as u32;
            string_table.extend_from_slice(name.as_bytes());
            string_table.push(0);
            entrypoints.push(EntryPoint {
                name: offset,
                target_entry: 0,
                args: 0,
                working_dir: WorkingDir::Inherit,
                flags: EntryPointFlags::empty(),
            });
        }
        Manifest {
            header: ManifestHeader {
                version: 3,
                entry_count: 0,
                string_table_size: string_table.len() as u32,
                entrypoint_count: entrypoints.len() as u16,
                default_entrypoint: 0,
                lib_dir_count: 0,
                name_offset: 0,
                package_id: [0; 32],
                prefix_hash: [0; 32],
                footer_hash: [0; 32],
            },
            entrypoints,
            entries: Vec::new(),
            lib_dir_offsets: Vec::new(),
            string_table,
        }
    }

    /// The labels of resolved pins, in resolution order.
    fn labels(pins: &[Pin]) -> Vec<&str> {
        pins.iter().map(|p| p.label.as_str()).collect()
    }

    #[test]
    fn a_legacy_table_selects_every_declared_pin_in_table_order() {
        let (qt, gtk) = ("ab".repeat(32), "cd".repeat(32));
        let table = unreachable_table(&[("qt", &qt), ("gtk", &gtk)]);
        let pins = selection(Some(&table), None, &manifest(&["app"]), "app").unwrap();
        assert_eq!(labels(&pins), ["qt", "gtk"]);
        assert_eq!(pins[0].blake3, qt);
        assert_eq!(pins[0].url, "file:///nonexistent/qt.onelf");
    }

    #[test]
    fn a_legacy_launch_without_a_table_or_a_record_acquires_nothing() {
        let pins = selection(None, None, &manifest(&["app"]), "app").unwrap();
        assert!(pins.is_empty());
    }

    #[test]
    fn an_empty_selection_acquires_nothing() {
        let table = unreachable_table(&[("gtk", &hex(&PIN_A)), ("qt", &hex(&PIN_B))]);
        let record = record(&[("gtk", PIN_A), ("qt", PIN_B)], &[("app", &[])]);
        let pins = selection(
            Some(&table),
            Some(&decoded(&record)),
            &manifest(&["app"]),
            "app",
        )
        .unwrap();
        assert!(pins.is_empty(), "{pins:?}");
    }

    #[test]
    fn a_partial_selection_obtains_exactly_the_named_pins() {
        let table = unreachable_table(&[("gtk", &hex(&PIN_A)), ("qt", &hex(&PIN_B))]);
        let record = record(
            &[("gtk", PIN_A), ("qt", PIN_B)],
            &[("app", &["qt"]), ("gui", &["gtk", "qt"])],
        );
        let manifest = manifest(&["app", "gui"]);
        let app = selection(Some(&table), Some(&decoded(&record)), &manifest, "app").unwrap();
        assert_eq!(labels(&app), ["qt"]);
        let gui = selection(Some(&table), Some(&decoded(&record)), &manifest, "gui").unwrap();
        assert_eq!(labels(&gui), ["gtk", "qt"], "the record's canonical order");
        assert_eq!(gui[0].blake3, hex(&PIN_A));
        assert_eq!(gui[1].blake3, hex(&PIN_B));
    }

    #[test]
    fn a_record_the_table_cannot_back_is_refused() {
        let table = unreachable_table(&[("qt", &hex(&PIN_A))]);
        let record = record(&[("qt", PIN_B)], &[("app", &["qt"])]);
        let err = selection(
            Some(&table),
            Some(&decoded(&record)),
            &manifest(&["app"]),
            "app",
        )
        .unwrap_err();
        assert!(err.contains("entrypoint-sets"), "{err}");
        assert!(err.contains("pin table"), "{err}");
    }

    #[test]
    fn a_record_for_a_manifest_it_does_not_match_is_refused() {
        let record = record(&[("qt", PIN_A)], &[("app", &["qt"]), ("ghost", &["qt"])]);
        let err = selection(None, Some(&decoded(&record)), &manifest(&["app"]), "app").unwrap_err();
        assert!(err.contains("manifest"), "{err}");
    }

    #[test]
    fn a_table_name_the_record_cannot_name_is_refused() {
        let table = unreachable_table(&[("-qt", &hex(&PIN_A))]);
        let record = record(&[], &[("app", &[])]);
        let err = selection(
            Some(&table),
            Some(&decoded(&record)),
            &manifest(&["app"]),
            "app",
        )
        .unwrap_err();
        assert!(err.contains("canonical set name"), "{err}");
    }

    #[test]
    fn a_record_without_a_table_declares_no_sets() {
        let record = record(&[], &[("app", &[])]);
        let pins = selection(None, Some(&decoded(&record)), &manifest(&["app"]), "app").unwrap();
        assert!(pins.is_empty());
    }

    /// Minimal stored-mode packages the runtime loader can open: the
    /// layout is `[prefix][manifest][stored payload][footer]`, one
    /// block per file, bytes verbatim. Every file path is split into
    /// real directory entries, so extraction's path validation accepts
    /// the tree.
    mod fixture {
        use std::collections::HashMap;

        use onelf_format::{
            Block, Entry, EntryKind, EntryPoint, EntryPointFlags, Flags, Footer, Manifest,
            ManifestHeader, WorkingDir,
        };

        /// A built package carrying `files`, whose entrypoints are
        /// `(name, target file path)` pairs. `lib_dirs` are the library
        /// directory names the manifest records, so a launch treats the
        /// package as carrying its own libraries.
        pub(crate) fn build(
            prefix: &[u8],
            package_id: [u8; 32],
            files: &[(&str, Vec<u8>)],
            entrypoints: &[(&str, &str)],
            lib_dirs: &[&str],
        ) -> Vec<u8> {
            fn put_text(value: &str, string_table: &mut Vec<u8>) -> u32 {
                let at = string_table.len() as u32;
                string_table.extend_from_slice(value.as_bytes());
                string_table.push(0);
                at
            }

            fn dir_index(
                entries: &mut Vec<Entry>,
                dirs: &mut HashMap<String, u32>,
                path: &str,
                string_table: &mut Vec<u8>,
            ) -> u32 {
                if path.is_empty() {
                    return u32::MAX;
                }
                if let Some(&index) = dirs.get(path) {
                    return index;
                }
                let (parent_path, name) = path.rsplit_once('/').unwrap_or(("", path));
                let parent = dir_index(entries, dirs, parent_path, string_table);
                let index = entries.len() as u32;
                entries.push(Entry {
                    kind: EntryKind::Dir,
                    parent,
                    name: put_text(name, string_table),
                    mode: 0o755,
                    mtime_secs: 0,
                    mtime_nsec: 0,
                    content_hash: [0; 32],
                    blocks: Vec::new(),
                    symlink_target: 0,
                });
                dirs.insert(path.to_string(), index);
                index
            }

            let mut string_table: Vec<u8> = vec![0];
            let mut entries: Vec<Entry> = Vec::new();
            let mut dirs: HashMap<String, u32> = HashMap::new();
            let mut payload = Vec::new();
            let mut file_index: HashMap<&str, usize> = HashMap::new();
            for (path, content) in files {
                let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
                let parent = dir_index(&mut entries, &mut dirs, dir, &mut string_table);
                file_index.insert(path, entries.len());
                let offset = payload.len() as u64;
                payload.extend_from_slice(content);
                let hash = *blake3::hash(content).as_bytes();
                entries.push(Entry {
                    kind: EntryKind::File,
                    parent,
                    name: put_text(name, &mut string_table),
                    mode: 0o755,
                    mtime_secs: 0,
                    mtime_nsec: 0,
                    content_hash: hash,
                    blocks: vec![Block {
                        payload_offset: offset,
                        compressed_size: content.len() as u64,
                        original_size: content.len() as u64,
                        content_hash: hash,
                    }],
                    symlink_target: 0,
                });
            }
            let eps: Vec<EntryPoint> = entrypoints
                .iter()
                .map(|(name, target)| EntryPoint {
                    name: put_text(name, &mut string_table),
                    target_entry: file_index[target] as u32,
                    args: 0,
                    working_dir: WorkingDir::Inherit,
                    flags: EntryPointFlags::empty(),
                })
                .collect();
            let lib_dir_offsets: Vec<u32> = lib_dirs
                .iter()
                .map(|dir| put_text(dir, &mut string_table))
                .collect();
            let manifest = Manifest {
                header: ManifestHeader {
                    version: 3,
                    entry_count: entries.len() as u32,
                    string_table_size: string_table.len() as u32,
                    entrypoint_count: eps.len() as u16,
                    default_entrypoint: 0,
                    lib_dir_count: lib_dir_offsets.len() as u16,
                    name_offset: 0,
                    package_id,
                    prefix_hash: *blake3::hash(prefix).as_bytes(),
                    footer_hash: [0; 32],
                },
                entrypoints: eps,
                entries,
                lib_dir_offsets,
                string_table,
            };
            let raw = manifest.serialize().unwrap();
            let compressed = zstd::bulk::compress(&raw, 1).unwrap();
            let manifest_offset = prefix.len() as u64;
            let payload_offset = manifest_offset + compressed.len() as u64;
            let footer = Footer {
                format_version: onelf_format::footer::FORMAT_VERSION,
                flags: Flags::STORED,
                manifest_offset,
                manifest_compressed: compressed.len() as u64,
                manifest_original: raw.len() as u64,
                payload_offset,
                payload_size: payload.len() as u64,
                dict_offset: 0,
                dict_size: 0,
                manifest_checksum: onelf_format::manifest::manifest_checksum(&raw).unwrap(),
            };
            let mut bytes = Vec::new();
            bytes.extend_from_slice(prefix);
            bytes.extend_from_slice(&compressed);
            bytes.extend_from_slice(&payload);
            footer.write_to(&mut bytes).unwrap();
            bytes
        }
    }

    /// A case's private directories: where acquisition may publish
    /// builds (the store hint), where extraction may cache them, and
    /// where the packed fixture runs from.
    struct Sandbox {
        dir: PathBuf,
        store: PathBuf,
        cache: PathBuf,
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn sandbox(name: &str) -> Sandbox {
        let dir = std::env::temp_dir().join(format!(
            "onelf-sets-{name}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("run")).unwrap();
        fs::create_dir_all(dir.join("store")).unwrap();
        fs::create_dir_all(dir.join("cache")).unwrap();
        Sandbox {
            store: dir.join("store"),
            cache: dir.join("cache"),
            dir,
        }
    }

    /// The built runtime's bytes: the executable prefix every packed
    /// fixture runs as.
    fn runtime_bytes() -> Vec<u8> {
        let from_cargo = std::env::var("CARGO_BIN_EXE_onelf-rt")
            .ok()
            .map(PathBuf::from);
        let from_test_exe = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.ancestors().nth(2).map(|dir| dir.join("onelf-rt")));
        let path = from_cargo
            .or(from_test_exe)
            .unwrap_or_else(|| panic!("cannot locate the built onelf-rt binary"));
        std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    /// A stored package a pin can name, written to `path`, with the
    /// digest its pin must carry.
    fn build_artifact(path: &Path, package_id: [u8; 32]) -> [u8; 32] {
        let bytes = fixture::build(
            b"onelf fixture prefix",
            package_id,
            &[("lib/libfake.so.1", b"set bytes".to_vec())],
            &[],
            &[],
        );
        fs::write(path, &bytes).unwrap();
        *blake3::hash(&bytes).as_bytes()
    }

    /// Run a fixture as a packed executable: the built runtime is the
    /// prefix, the fixture is its package, and the child's exit code and
    /// stderr report how far the launch got. The payload is a script
    /// that exits 7, so a launch that gets past acquisition runs to a
    /// deterministic end, the mode is forced to rundir, and extraction
    /// mode is forced for the sets so no helper or mount joins the
    /// picture. `lib_dirs` are the library directories the package's
    /// manifest records.
    fn launch(
        name: &str,
        records: &[(&str, Vec<u8>)],
        sandbox: &Sandbox,
        fetch_prohibited: bool,
    ) -> (Option<i32>, String) {
        launch_with_lib_dirs(name, records, &[], sandbox, fetch_prohibited)
    }

    /// [`launch`] over a package that records `lib_dirs` and carries the
    /// extra files, so a launch has a bundled libc and loader of its own
    /// to answer providers against.
    fn launch_with_lib_dirs(
        name: &str,
        records: &[(&str, Vec<u8>)],
        lib_dirs: &[&str],
        sandbox: &Sandbox,
        fetch_prohibited: bool,
    ) -> (Option<i32>, String) {
        let _forks = crate::test_locks::forking();
        let runtime = runtime_bytes();
        let mut files: Vec<(&str, Vec<u8>)> = vec![("bin/app", b"#!/bin/sh\nexit 7\n".to_vec())];
        files.extend_from_slice(records);
        let package = fixture::build(
            &runtime,
            [0x5e; 32],
            &files,
            &[("app", "bin/app")],
            lib_dirs,
        );
        let exe = sandbox.dir.join("run").join(name);
        fs::write(&exe, &package).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        let mut command = Command::new(&exe);
        command
            .env_remove("ONELF_ENTRYPOINT_EXPORTED")
            .env_remove("ONELF_CACHE")
            .env("ONELF_ENTRYPOINT", "app")
            .env("ONELF_MODE", "rundir")
            .env("ONELF_SET_MODE", "extract")
            .env("ONELF_NO_RESOLVER", "1")
            .env("ONELF_PLATFORM_STORE", &sandbox.store)
            .env("XDG_CACHE_HOME", &sandbox.cache);
        if fetch_prohibited {
            command.env("ONELF_NO_PLATFORM_FETCH", "1");
        } else {
            command.env_remove("ONELF_NO_PLATFORM_FETCH");
        }
        let output = command.output().unwrap();
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    /// Whether `root` holds no file at all. A directory that does not
    /// exist is empty.
    fn tree_is_empty(root: &Path) -> bool {
        let Ok(entries) = fs::read_dir(root) else {
            return true;
        };
        entries.flatten().all(|entry| {
            entry.file_type().is_ok_and(|t| t.is_dir()) && tree_is_empty(&entry.path())
        })
    }

    /// An entrypoint that selects nothing runs with zero toolkit-set
    /// fetches, stores or extractions. Both declared sets are fully
    /// acquirable here, so any attempt would publish one.
    #[test]
    fn an_empty_selection_touches_no_toolkit_artifact() {
        let sandbox = sandbox("empty");
        let (src_qt, src_gtk) = (sandbox.dir.join("src-qt"), sandbox.dir.join("src-gtk"));
        let (qt, gtk) = (
            build_artifact(&src_qt, [1; 32]),
            build_artifact(&src_gtk, [2; 32]),
        );
        let sets = table(&[
            ("qt", &file_url(&src_qt), &hex(&qt)),
            ("gtk", &file_url(&src_gtk), &hex(&gtk)),
        ]);
        let oset = record(&[("qt", qt), ("gtk", gtk)], &[("app", &[])]);
        let (code, stderr) = launch(
            "empty",
            &[
                (crate::sets::SETS_FILE, sets),
                (crate::pkgmeta::ENTRYPOINT_SETS_FILE, oset),
            ],
            &sandbox,
            false,
        );
        assert!(
            !stderr.contains("dependency set"),
            "an empty selection must acquire nothing: {stderr}"
        );
        assert!(
            tree_is_empty(&sandbox.store),
            "the toolkit store must be untouched"
        );
        assert!(
            tree_is_empty(&sandbox.cache),
            "no set build may be extracted"
        );
        // The launch went past acquisition and the application ran to its
        // own deterministic exit.
        assert_eq!(code, Some(7), "{stderr}");
        assert!(!stderr.contains("dependency set"), "{stderr}");
    }

    /// A partial selection obtains exactly its canonical pin. The
    /// unselected set's source does not exist here, so any attempt at it
    /// would fail the launch and name it.
    #[test]
    fn a_partial_selection_obtains_its_pin_and_ignores_unavailable_unselected_sets() {
        let sandbox = sandbox("partial");
        let src_qt = sandbox.dir.join("src-qt");
        let qt = build_artifact(&src_qt, [3; 32]);
        let sets = table(&[
            ("qt", &file_url(&src_qt), &hex(&qt)),
            ("gtk", "file:///nonexistent/gtk.onelf", &hex(&[0x77; 32])),
        ]);
        let oset = record(&[("qt", qt), ("gtk", [0x77; 32])], &[("app", &["qt"])]);
        let (code, stderr) = launch(
            "partial",
            &[
                (crate::sets::SETS_FILE, sets),
                (crate::pkgmeta::ENTRYPOINT_SETS_FILE, oset),
            ],
            &sandbox,
            false,
        );
        let stored = sandbox.store.join(format!("qt/{}.onelf", hex(&qt)));
        assert!(
            stored.is_file(),
            "the selected pin must be obtained: {stderr}"
        );
        assert!(
            !sandbox.store.join("gtk").exists(),
            "an unselected set must not be attempted"
        );
        assert!(
            !stderr.contains("gtk"),
            "the unselected set must be silent: {stderr}"
        );
        assert_eq!(code, Some(7), "{stderr}");
    }

    /// A required selected set that cannot be had ends the launch
    /// naming the entrypoint and the pin, publishing nothing, and
    /// nothing continues under the optional GL build's warning policy.
    #[test]
    fn a_required_selected_set_that_cannot_be_obtained_ends_the_launch() {
        let sandbox = sandbox("required");
        let sets = unreachable_table(&[("qt", &hex(&PIN_A))]);
        let oset = record(&[("qt", PIN_A)], &[("app", &["qt"])]);
        let (code, stderr) = launch(
            "required",
            &[
                (crate::sets::SETS_FILE, sets),
                (crate::pkgmeta::ENTRYPOINT_SETS_FILE, oset),
            ],
            &sandbox,
            true,
        );
        assert_eq!(code, Some(1), "{stderr}");
        assert!(stderr.contains("dependency set qt"), "{stderr}");
        assert!(stderr.contains("required by entrypoint app"), "{stderr}");
        assert!(
            !stderr.contains("continuing without"),
            "a required set's failure must not degrade into a warning: {stderr}"
        );
        assert!(tree_is_empty(&sandbox.store));
    }

    /// A path overlay record that does not decode skips the overlays,
    /// naming the refused pair, and the launch still runs the payload.
    #[test]
    fn an_invalid_path_record_skips_the_overlays_and_launches() {
        let sandbox = sandbox("hostpaths");
        let (code, stderr) = launch(
            "hostpaths",
            &[(
                onelf_format::paths::PATHS_FILE,
                b"/usr/share/y\t../outside".to_vec(),
            )],
            &sandbox,
            true,
        );
        assert_eq!(code, Some(7), "{stderr}");
        assert!(stderr.contains("not overlaying"), "{stderr}");
        assert!(
            stderr.contains("\"/usr/share/y\" = \"../outside\""),
            "{stderr}"
        );
    }

    /// A record whose pins the carried set table does not back ends the
    /// launch before any acquisition, even though its own tables are
    /// internally consistent.
    #[test]
    fn a_record_the_set_table_cannot_back_ends_the_launch_before_acquisition() {
        let sandbox = sandbox("backing");
        let sets = unreachable_table(&[("qt", &hex(&PIN_A))]);
        let oset = record(&[("qt", PIN_B)], &[("app", &["qt"])]);
        let (code, stderr) = launch(
            "backing",
            &[
                (crate::sets::SETS_FILE, sets),
                (crate::pkgmeta::ENTRYPOINT_SETS_FILE, oset),
            ],
            &sandbox,
            true,
        );
        assert_eq!(code, Some(1), "{stderr}");
        assert!(stderr.contains("entrypoint-sets"), "{stderr}");
        assert!(stderr.contains("does not match"), "{stderr}");
        assert!(!stderr.contains("dependency set"), "{stderr}");
        assert!(tree_is_empty(&sandbox.store));
    }

    /// A package without a selection record keeps the legacy behavior:
    /// every declared set is required, in table order.
    #[test]
    fn a_legacy_package_requires_every_declared_set_in_table_order() {
        let sandbox = sandbox("legacy");
        let sets = unreachable_table(&[("qt", &hex(&PIN_A)), ("gtk", &hex(&PIN_B))]);
        let (code, stderr) = launch("legacy", &[(crate::sets::SETS_FILE, sets)], &sandbox, true);
        assert_eq!(code, Some(1), "{stderr}");
        assert!(stderr.contains("dependency set qt"), "{stderr}");
        assert!(stderr.contains("required by entrypoint app"), "{stderr}");
        assert!(
            !stderr.contains("gtk"),
            "the launch stops at the first required set: {stderr}"
        );
    }

    /// A stored artifact whose library is the ELF bytes `lib` names, with
    /// the digest its pin must carry.
    fn build_elf_artifact(path: &Path, lib: Vec<u8>) -> [u8; 32] {
        let bytes = fixture::build(
            b"onelf fixture prefix",
            [0x11; 32],
            &[("lib/libfake.so.1", lib)],
            &[],
            &[],
        );
        fs::write(path, &bytes).unwrap();
        *blake3::hash(&bytes).as_bytes()
    }

    /// The glibc-shaped libc a package bundles for these cases: machine
    /// 62, defining GLIBC_2.2.5 and nothing newer.
    fn bundled_libc() -> Vec<u8> {
        use crate::providers::fixture::elf64;
        elf64(
            62,
            Some("libc.so.6"),
            &["ld-linux-x86-64.so.2"],
            &[(1, "libc.so.6"), (0, "GLIBC_2.2.5")],
            &[],
        )
    }

    /// An ELF64 set object with the given machine, dependencies,
    /// definitions and version requirements.
    fn set_object(
        machine: u16,
        needed: &[&str],
        definitions: &[(u16, &str)],
        requirements: &[(&str, &[(u16, &str)])],
    ) -> Vec<u8> {
        use crate::providers::fixture::elf64;
        elf64(
            machine,
            Some("libfake.so.1"),
            needed,
            definitions,
            requirements,
        )
    }

    /// An old set artifact, published before provider indexes existed,
    /// keeps working without republishing: the launch reads its objects
    /// as they stand, and one whose objects this launch can account for
    /// runs to its own end.
    #[test]
    fn an_old_artifact_launches_without_republishing() {
        let sandbox = sandbox("old-artifact");
        let src = sandbox.dir.join("src-qt");
        let qt = build_elf_artifact(&src, set_object(62, &[], &[(1, "libfake.so.1")], &[]));
        let sets = table(&[("qt", &file_url(&src), &hex(&qt))]);
        let oset = record(&[("qt", qt)], &[("app", &["qt"])]);
        let (code, stderr) = launch(
            "old-artifact",
            &[
                (crate::sets::SETS_FILE, sets),
                (crate::pkgmeta::ENTRYPOINT_SETS_FILE, oset),
            ],
            &sandbox,
            false,
        );
        assert_eq!(code, Some(7), "{stderr}");
        assert!(
            sandbox
                .store
                .join(format!("qt/{}.onelf", hex(&qt)))
                .is_file()
        );
        assert!(!stderr.contains("cannot run"), "{stderr}");
    }

    /// A selected set whose ELF data is truncated ends the launch during
    /// preparation, naming the set and the object, before the
    /// application executes.
    #[test]
    fn a_truncated_provider_object_ends_the_launch_before_the_application_runs() {
        let sandbox = sandbox("truncated-set");
        let src = sandbox.dir.join("src-qt");
        let mut lib = set_object(62, &[], &[(1, "libfake.so.1")], &[]);
        lib.truncate(lib.len() - 8);
        let qt = build_elf_artifact(&src, lib);
        let sets = table(&[("qt", &file_url(&src), &hex(&qt))]);
        let oset = record(&[("qt", qt)], &[("app", &["qt"])]);
        let (code, stderr) = launch(
            "truncated-set",
            &[
                (crate::sets::SETS_FILE, sets),
                (crate::pkgmeta::ENTRYPOINT_SETS_FILE, oset),
            ],
            &sandbox,
            false,
        );
        assert_eq!(code, Some(1), "{stderr}");
        assert!(stderr.contains("dependency set qt"), "{stderr}");
        assert!(stderr.contains("required by entrypoint app"), "{stderr}");
        assert!(stderr.contains("lib/libfake.so.1"), "{stderr}");
        assert!(!stderr.contains("exit 7"), "{stderr}");
    }

    /// A set object that needs a libc symbol version newer than the
    /// bundled pair, or than any selected set, ends the launch before
    /// the application executes.
    #[test]
    fn a_libc_requirement_beyond_the_bundled_libc_ends_the_launch() {
        let sandbox = sandbox("newer-libc");
        let src = sandbox.dir.join("src-qt");
        let qt = build_elf_artifact(
            &src,
            set_object(
                62,
                &["libc.so.6"],
                &[(1, "libfake.so.1")],
                &[("libc.so.6", &[(0, "GLIBC_2.999")])],
            ),
        );
        let sets = table(&[("qt", &file_url(&src), &hex(&qt))]);
        let oset = record(&[("qt", qt)], &[("app", &["qt"])]);
        let (code, stderr) = launch_with_lib_dirs(
            "newer-libc",
            &[
                ("lib/libc.so.6", bundled_libc()),
                (crate::sets::SETS_FILE, sets),
                (crate::pkgmeta::ENTRYPOINT_SETS_FILE, oset),
            ],
            &["lib"],
            &sandbox,
            false,
        );
        assert_eq!(code, Some(1), "{stderr}");
        assert!(stderr.contains("GLIBC_2.999"), "{stderr}");
        assert!(stderr.contains("entrypoint app"), "{stderr}");
    }

    /// A provider object built for another machine ends the launch
    /// naming the object, before the application executes.
    #[test]
    fn a_wrong_architecture_provider_ends_the_launch() {
        let sandbox = sandbox("wrong-arch");
        let src = sandbox.dir.join("src-qt");
        let qt = build_elf_artifact(&src, set_object(3, &[], &[(1, "libfake.so.1")], &[]));
        let sets = table(&[("qt", &file_url(&src), &hex(&qt))]);
        let oset = record(&[("qt", qt)], &[("app", &["qt"])]);
        let (code, stderr) = launch_with_lib_dirs(
            "wrong-arch",
            &[
                ("lib/libc.so.6", bundled_libc()),
                (crate::sets::SETS_FILE, sets),
                (crate::pkgmeta::ENTRYPOINT_SETS_FILE, oset),
            ],
            &["lib"],
            &sandbox,
            false,
        );
        assert_eq!(code, Some(1), "{stderr}");
        assert!(stderr.contains("machine 3"), "{stderr}");
        assert!(stderr.contains("lib/libfake.so.1"), "{stderr}");
    }

    /// A legacy package whose every declared set is obtainable and
    /// runnable keeps its documented default: it launches with every
    /// set obtained, without any selection record.
    #[test]
    fn a_legacy_package_runs_with_every_declared_set_obtained() {
        let sandbox = sandbox("legacy-runs");
        let (src_qt, src_gtk) = (sandbox.dir.join("src-qt"), sandbox.dir.join("src-gtk"));
        let (qt, gtk) = (
            build_artifact(&src_qt, [4; 32]),
            build_artifact(&src_gtk, [5; 32]),
        );
        let sets = table(&[
            ("qt", &file_url(&src_qt), &hex(&qt)),
            ("gtk", &file_url(&src_gtk), &hex(&gtk)),
        ]);
        let (code, stderr) = launch(
            "legacy-runs",
            &[(crate::sets::SETS_FILE, sets)],
            &sandbox,
            false,
        );
        assert_eq!(code, Some(7), "{stderr}");
        assert!(
            sandbox
                .store
                .join(format!("qt/{}.onelf", hex(&qt)))
                .is_file()
        );
        assert!(
            sandbox
                .store
                .join(format!("gtk/{}.onelf", hex(&gtk)))
                .is_file()
        );
    }
}

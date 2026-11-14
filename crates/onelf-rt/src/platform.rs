//! The GL build a package pins, for a host that has no GL stack of its
//! own.
//!
//! The package records the build's label, URL and BLAKE3 hash in
//! `.onelf/platform`. The build is fetched once per label into the
//! shared store under the cache root, and every acquisition verifies the
//! stored bytes against that hash through a single held file handle
//! before anything extracts or serves them, so two packages pinning the
//! same build share both the download and the verification. Extraction
//! goes through the ordinary package cache. Every failure is reported by
//! the caller as a warning and the launch goes on without a GL stack.
//!
//! Where the store lives is decided by the target uid alone. An ordinary
//! launch keeps its explicit relocation when the relocated store is its
//! own, a privileged launch keeps only its own private base, and the
//! inherited `ONELF_PLATFORM_STORE` hint never overrides that ownership
//! decision. A hint the target does not own is import input instead.
//!
//! Acquisition tries three opportunities in order: the verified copy in
//! the target's own store, an import from the hinted store, and a fetch
//! from the pin's configured source unless `ONELF_NO_PLATFORM_FETCH`
//! forbids it. A rejected opportunity is unavailable, never fatal on its
//! own; the error names every refusal once the last one is exhausted.
//!
//! A stored candidate is pinned before anything reads it: the
//! store-relative name is walked one component at a time with
//! `O_PATH|O_NOFOLLOW`, must be a regular file by the `fstat` of its
//! pinned descriptor, and is converted to a readable handle for the same
//! inode without another lookup through the store, so a symlink planted
//! inside the store, a name swapped while the candidate is held, or a
//! FIFO, device, socket or directory at the stored name never activates
//! content I/O. The store root itself is reached by name and may be a
//! symlink: it is the location the target uid owns or chose. An import
//! source is pinned the same way inside the hinted store, and nothing in
//! that store is ever written, renamed, chmod'd or touched.
//!
//! Publication into the store is locked, staged, and verified. An
//! acquirer of a missing build takes the store's exclusive per-label
//! flock, stages the imported or fetched bytes in an exclusive scratch
//! file created by descriptor inside the pinned label directory, bounds
//! the copy so a source that never ends cannot fill the disk, and
//! verifies the scratch's own bytes, read back through the descriptor
//! they were written through, against the pin and as a container before
//! one atomic rename publishes them. The loser of the race re-verifies
//! the winner's publication and uses it: trust stays with the digest,
//! never with who wrote the bytes. A privileged target stages privately
//! under validated root-owned directories, and refuses any pre-existing
//! stored name whose owner or permissions would leave an untrusted uid
//! write access, repairing it by the same rename, which never opens what
//! it replaces.

use std::fs;
use std::io::{self, Read, Seek};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use rustix::fs::{FileType, Mode, OFlags, fstat};
use rustix::io::Errno;

/// An HTTPS transfer a caller supplies: stream `url` into the already
/// open exclusive staging file, refusing the transfer as soon as it
/// exceeds the byte bound, or fail with a description of what went
/// wrong. The library carries no client of its own, so every embedding
/// process hands its existing client over here and nothing in the
/// library's base dependencies pulls an HTTPS stack in. The transfer
/// reports the bytes it has written, and the total when the server says,
/// through the progress callback.
pub type HttpsFetch =
    fn(&str, &mut fs::File, u64, &mut dyn FnMut(u64, Option<u64>)) -> Result<(), String>;

static HTTPS_FETCH: OnceLock<HttpsFetch> = OnceLock::new();

/// Supply the HTTPS operation acquisition uses for `https://` sources.
/// The first registration in the process wins; later ones are ignored,
/// because one process drives one launch-support stack with one client.
/// Without any registration, `https://` sources are refused and only
/// `file://` acquisition works.
pub fn supply_https_fetch(fetch: HttpsFetch) {
    let _ = HTTPS_FETCH.set(fetch);
}

/// Where the package records its pin, relative to the package root.
pub const PIN_FILE: &str = ".onelf/platform";

/// A GL build pinned by hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    /// The label the package stores the build under.
    pub label: String,
    /// The source the build is fetched from.
    pub url: String,
    /// The BLAKE3 digest the stored build must hash to.
    pub blake3: String,
}

/// Parse the record's `key = "value"` lines. Values are TOML basic
/// strings as the packer writes them, with `\"` and `\\` escapes.
pub fn parse_pin(text: &str) -> Option<Pin> {
    let (mut label, mut url, mut blake3) = (None, None, None);
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        let value = value.strip_prefix('"')?.strip_suffix('"')?;
        let value = value.replace("\\\"", "\"").replace("\\\\", "\\");
        match key.trim() {
            "label" => label = Some(value),
            "url" => url = Some(value),
            "blake3" => blake3 = Some(value.to_ascii_lowercase()),
            _ => {}
        }
    }
    checked_pin(label?, url?, blake3?)
}

/// A pin, if the three values can name a stored build: the label is a
/// directory name and the hash names the file and is compared against
/// its bytes, so it has to be exactly one BLAKE3 digest in hex.
pub(crate) fn checked_pin(label: String, url: String, blake3: String) -> Option<Pin> {
    if label.is_empty() || label.contains('/') || label.starts_with('.') {
        return None;
    }
    if blake3.len() != 64 || !blake3.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(Pin { label, url, blake3 })
}

fn env_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty() && v != "0")
}

/// Make the build the package at `pkg_root` pins available: fetched if
/// the store lacks it, verified, extracted. Returns the extracted root
/// and the lock that keeps the extraction from being collected while
/// this instance runs, or `None` when the package pins no build. The
/// error names why the pinned build could not be used.
///
/// `ONELF_PLATFORM_URL` replaces the pinned source for an ordinary uid
/// only. A privileged launch keeps the package's own source, since the
/// variable was chosen by the unprivileged caller that started it.
///
/// Public for a caller that prepares a launch outside a packed package,
/// as the CLI's development run does: the pin lives in the AppDir's
/// `.onelf/platform`, and the caller owns the warn-and-continue policy
/// the packed launch applies to this result.
pub fn obtain(pkg_root: &Path) -> Option<Result<(PathBuf, fs::File), String>> {
    let text = fs::read_to_string(pkg_root.join(PIN_FILE)).ok()?;
    let Some(mut pin) = parse_pin(&text) else {
        return Some(Err(format!("{PIN_FILE} names no usable build")));
    };
    if crate::paths::uid() != 0
        && let Some(url) = std::env::var("ONELF_PLATFORM_URL")
            .ok()
            .filter(|u| !u.is_empty())
    {
        pin.url = url;
    }

    Some(obtain_build(&pin))
}

/// Make the build `pin` names available: fetched into the shared store if
/// it is not there, verified against the hash before it is used, then
/// extracted through the ordinary package cache. Returns the extracted
/// root and the lock that keeps it from being collected while this
/// instance runs.
///
/// Shared by the GL build and by every dependency set, which differ only
/// in when they are wanted, not in how they are obtained.
pub(crate) fn obtain_build(pin: &Pin) -> Result<(PathBuf, fs::File), String> {
    let artifact = store_build(pin)?;
    extract_build(&artifact)
}

/// A cached build opened once and verified against its pin.
///
/// The handle owns the open file description the digest was computed
/// over, so every consumer after [`store_build`] reads the bytes that
/// were verified: extraction and the set mount server load through a
/// duplicate of the handle, and a file swapped in at the stored name
/// afterwards is never seen. The stored path is kept for messages only.
#[derive(Debug)]
pub(crate) struct VerifiedArtifact {
    path: PathBuf,
    file: fs::File,
}

impl VerifiedArtifact {
    /// A duplicate of the verified handle, for a consumer that loads the
    /// container itself.
    pub(crate) fn handle(&self) -> Result<fs::File, String> {
        self.file
            .try_clone()
            .map_err(|e| format!("{}: {e}", self.path.display()))
    }

    /// The stored path the verified handle was published or found at.
    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

/// Extract the build `artifact` holds through the ordinary package cache.
/// The container is loaded from a duplicate of the verified handle, so
/// the bytes are the ones whose digest matched the pin.
pub(crate) fn extract_build(artifact: &VerifiedArtifact) -> Result<(PathBuf, fs::File), String> {
    let mut pkg = crate::loader::load_file(artifact.handle()?)
        .map_err(|e| format!("{}: not a usable build: {e}", artifact.path.display()))?;
    crate::cache::ensure_extracted(&mut pkg)
        .map_err(|e| format!("{}: cannot extract: {e}", artifact.path.display()))
}

/// The environment name the launch exports the resolved ordinary store
/// under, and the only name acquisition reads an import hint from.
pub(crate) const STORE_HINT_VAR: &str = "ONELF_PLATFORM_STORE";

/// The inherited import hint: where the launch that started this one
/// resolved its ordinary store, exported so the same pinned build is not
/// downloaded twice. A suggestion only. It names the store an ordinary
/// target owns, or else a store to import from, and it never overrides
/// the ownership decision: the name arrived through the environment of
/// the invoking user, so a target that does not own it refuses it as a
/// destination, and no guarantee depends on a privilege transition
/// keeping it.
fn import_hint() -> Option<PathBuf> {
    std::env::var_os(STORE_HINT_VAR)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Whether the store root `dir` belongs to `uid`: the directory it opens
/// to, following a symlink at the root as the store root always does, is
/// owned by `uid`. A store that does not exist yet is judged by its
/// nearest existing ancestor, so a fresh path under this uid's own tree
/// is accepted while one under another uid's home stays import input.
fn store_owned_by(dir: &Path, uid: u32) -> bool {
    let mut probe = dir;
    loop {
        match rustix::fs::open(
            probe,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => return fstat(&fd).is_ok_and(|st| st.st_uid == uid),
            Err(Errno::NOENT) => match probe.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => probe = parent,
                _ => return false,
            },
            Err(_) => return false,
        }
    }
}

/// The store directory the target uid owns, from the uid's decision over
/// the inherited hint and the candidate bases.
///
/// An ordinary uid keeps its relocation when the relocated store root is
/// its own: the hint then names the store this user chose. A hint naming
/// another uid's store, as `sudo -u other -E` leaves behind, is import
/// input, and the per-user cache serves as the destination, as it does
/// without a hint. A privileged uid takes only its own private base.
/// Every environment-derived name root could otherwise write through,
/// the hint as well as the HOME/XDG-derived cache root, was chosen by
/// the unprivileged caller, so both are refused, and neither candidate
/// base is read unless the decision needs it.
///
/// The bases are closures so a refusal costs no probe: resolving root's
/// store must not create or stat anything under the paths the
/// environment names.
pub(crate) fn owned_store(
    uid: u32,
    hint: Option<&Path>,
    cache_base: impl FnOnce() -> Option<PathBuf>,
    private_base: impl FnOnce() -> Option<PathBuf>,
) -> Result<PathBuf, String> {
    let hint = hint.filter(|v| !v.as_os_str().is_empty());
    if uid != 0 {
        if let Some(dir) = hint
            && store_owned_by(dir, uid)
        {
            return Ok(dir.to_path_buf());
        }
        return cache_base()
            .map(|base| base.join("platform"))
            .ok_or_else(|| {
                "no cache directory to store a build in (set HOME or XDG_CACHE_HOME)".to_string()
            });
    }
    private_base()
        .map(|base| base.join("platform"))
        .ok_or_else(|| "no root-owned private directory to store a build in".to_string())
}

/// The store this launch resolves to, the decision [`owned_store`] makes
/// for the target uid: an ordinary uid's own hinted store or per-user
/// cache, a privileged uid's own private base. `None` when no safe
/// location resolves, which is the launch that has nowhere to put a
/// build. The launch environment exports it under [`STORE_HINT_VAR`] so
/// the processes the app spawns see where the store resolves; the export
/// is a location only, and every acquisition re-derives its destination
/// from the target uid instead of trusting it.
///
/// The resolution is read-only: an ordinary uid without an owned hint
/// gets the location the environment proposes, without creating or
/// probing the persistent cache, so a launch that never acquires leaves
/// it untouched. Acquisition creates whatever it needs on demand through
/// the checked creation rules, and re-derives the destination from the
/// uid rather than from this export.
pub(crate) fn resolved_store() -> Option<PathBuf> {
    let uid = crate::paths::uid();
    let hint = import_hint();
    if uid != 0 {
        if let Some(dir) = hint
            && store_owned_by(&dir, uid)
        {
            return Some(dir);
        }
        return onelf_format::cache_layout::proposed_root(uid, None)
            .filter(|root| {
                !root.exists() || onelf_format::cache_layout::is_safe_owned_dir(root, uid)
            })
            .map(|base| base.join("platform"));
    }
    owned_store(
        uid,
        hint.as_deref(),
        crate::cache::base_dir,
        crate::paths::private_dir,
    )
    .ok()
}

/// Where the shared store keeps the build `pin` names, whether or not it
/// is there yet. The store is the one the target uid owns; see
/// [`owned_store`] for the ownership decision.
pub fn stored_path(pin: &Pin) -> Result<PathBuf, String> {
    let (store, relative) = stored_location(pin)?;
    Ok(store.join(relative))
}

/// [`stored_path`] split into the store root and the store-relative
/// candidate the confined walk needs: the label directory and the
/// hash-named file. The name proves nothing about the bytes: every
/// acquisition verifies them against the pin before use.
fn stored_location(pin: &Pin) -> Result<(PathBuf, String), String> {
    let hint = import_hint();
    let store = owned_store(
        crate::paths::uid(),
        hint.as_deref(),
        crate::cache::base_dir,
        crate::paths::private_dir,
    )?;
    // The build is stored by its own hash, so two builds a label pins over
    // its lifetime coexist rather than overwrite each other and force a
    // refetch on every launch.
    Ok((store, format!("{}/{}.onelf", pin.label, pin.blake3)))
}

/// The build `pin` names, from the shared store: the cached file is
/// pinned and hashed through confined descriptors, and only a missing or
/// mismatching file is imported from a hinted store the target does not
/// own, or fetched, then verified and published in its place. Returns
/// the verified handle every consumer reads through.
pub(crate) fn store_build(pin: &Pin) -> Result<VerifiedArtifact, String> {
    let (store, relative) = stored_location(pin)?;
    let hint = import_hint();
    let artifact = acquire(
        &store,
        &relative,
        pin,
        !env_set("ONELF_NO_PLATFORM_FETCH"),
        crate::paths::uid(),
        hint.as_deref().filter(|h| *h != store),
    )?;
    note_used_store(&store);
    Ok(artifact)
}

/// The stores this process used a build from, which [`sweep_used_stores`]
/// sweeps once the launch holds every build it needs.
static USED_STORES: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

fn note_used_store(store: &Path) {
    let mut used = USED_STORES.lock().unwrap_or_else(|e| e.into_inner());
    if !used.iter().any(|s| s == store) {
        used.push(store.to_path_buf());
    }
}

/// Remove the idle builds of every store this launch used a build from,
/// once it has all of them: they carry a fresh use, and a build untouched
/// for `ONELF_GC_MAX_AGE` days goes. Best effort, and skipped when that
/// age is 0 or fetching is disabled, since a build removed then could not
/// be had again. Label directories stay, as another launch may be about
/// to stage into one.
pub(crate) fn sweep_used_stores() {
    let stores = std::mem::take(&mut *USED_STORES.lock().unwrap_or_else(|e| e.into_inner()));
    let days = crate::cache::gc_max_age_days();
    if days == 0 || env_set("ONELF_NO_PLATFORM_FETCH") {
        return;
    }
    for store in stores {
        sweep_idle_builds(
            &store,
            std::time::Duration::from_secs(days.saturating_mul(86400)),
            false,
        );
    }
}

/// A build the store holds: its label, the hash it is named by, its file,
/// when a package last used it, and its size. The store keys a build by
/// its hash, so one label may hold several as its pin moves on.
pub struct StoredBuild {
    /// The label directory it is stored under.
    pub label: String,
    /// The BLAKE3 digest its file is named by.
    pub hash: String,
    /// Its path.
    pub file: PathBuf,
    /// Its last use: every use touches the file.
    pub used: Option<std::time::SystemTime>,
    /// Its size in bytes.
    pub bytes: u64,
}

/// The builds `store` holds: every `<hash>.onelf` under every label
/// directory, the hash a BLAKE3 digest in lowercase hex. Anything else,
/// a partial download included, is not a build.
pub fn stored_builds(store: &Path) -> Vec<StoredBuild> {
    let Ok(labels) = fs::read_dir(store) else {
        return Vec::new();
    };
    let mut builds: Vec<StoredBuild> = Vec::new();
    for label in labels.flatten() {
        if !label.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let label_name = label.file_name().to_string_lossy().into_owned();
        let Ok(files) = fs::read_dir(label.path()) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            let Some(hash) = name.strip_suffix(".onelf") else {
                continue;
            };
            if hash.len() != 64 || !hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
                continue;
            }
            let md = file.metadata().ok();
            builds.push(StoredBuild {
                label: label_name.clone(),
                hash: hash.to_string(),
                file: file.path(),
                used: md.as_ref().and_then(|m| m.modified().ok()),
                bytes: md.map(|m| m.len()).unwrap_or(0),
            });
        }
    }
    builds.sort_by(|a, b| (&a.label, &a.hash).cmp(&(&b.label, &b.hash)));
    builds
}

/// Remove the builds in `store` unused for longer than `max_age`, and with
/// `empty_labels` each label directory its last build leaves empty. A
/// build whose use time lies in the future is kept. Returns how many
/// builds went and the bytes they held.
pub fn sweep_idle_builds(
    store: &Path,
    max_age: std::time::Duration,
    empty_labels: bool,
) -> (u64, u64) {
    let now = std::time::SystemTime::now();
    let (mut removed, mut bytes) = (0u64, 0u64);
    for build in stored_builds(store) {
        let idle = match build.used.map(|t| now.duration_since(t)) {
            None => true,
            Some(Ok(age)) => age > max_age,
            Some(Err(_)) => false,
        };
        if idle && fs::remove_file(&build.file).is_ok() {
            removed += 1;
            bytes += build.bytes;
            if empty_labels && let Some(parent) = build.file.parent() {
                let _ = fs::remove_dir(parent);
            }
        }
    }
    (removed, bytes)
}

/// [`store_build`] against an explicit store root, the store-relative
/// candidate name, a fetch policy, the target uid the store belongs to,
/// and the store to import from, so the acquisition the GL build and the
/// dependency sets share is testable without touching either's
/// environment.
fn acquire(
    store: &Path,
    relative: &str,
    pin: &Pin,
    allow_fetch: bool,
    uid: u32,
    import: Option<&Path>,
) -> Result<VerifiedArtifact, String> {
    let path = store.join(relative);
    let origin = path.display().to_string();
    let mut refused: Option<String> = None;
    if let Some(file) = own_copy(store, relative, &pin.blake3, &origin, uid, &mut refused)? {
        return Ok(verified(path, file));
    }
    let miss = |refused: &Option<String>, why: String| match refused {
        Some(refused) => format!("{refused}; {why}"),
        None => why,
    };
    let prohibited = || {
        format!(
            "fetching is disabled by ONELF_NO_PLATFORM_FETCH ({} would be fetched from {})",
            pin.label, pin.url
        )
    };
    if import.is_none() && !allow_fetch {
        return Err(miss(&refused, prohibited()));
    }
    let Some((label, name)) = relative.rsplit_once('/') else {
        return Err(miss(
            &refused,
            "the candidate names no label directory".to_string(),
        ));
    };
    ensure_store_dirs(uid, store, &store.join(label)).map_err(|why| miss(&refused, why))?;
    // One exclusive flock per label serializes the acquirers of the
    // label's builds, so exactly one stages and publishes. No other lock
    // is held while it is taken, and it is released before the build is
    // served, so a slow download holds up only the acquirers of the same
    // build.
    let _publication = publication_lock(store, label).map_err(|why| miss(&refused, why))?;
    // The winner of the race may have published while this acquirer
    // waited: re-verify the stored candidate, since trust comes from the
    // digest, never from who wrote the bytes.
    if let Some(file) = own_copy(store, relative, &pin.blake3, &origin, uid, &mut refused)? {
        return Ok(verified(path, file));
    }
    // Everything staging touches from here resolves inside the pinned
    // label directory: the scratch is created, verified and published by
    // descriptor, so a name swapped above the directory cannot steer the
    // staging or the publication.
    let dir = match PinnedDir::walk(store, &[label], &origin, (uid == 0).then_some(uid)) {
        Ok(dir) => dir,
        Err(PinError::Missing) => {
            return Err(miss(
                &refused,
                format!("{origin}: the store directory went away before staging"),
            ));
        }
        Err(other) => return Err(miss(&refused, other.message())),
    };
    if let Some(hint) = import {
        match import_copy(&dir, hint, relative, name, pin, uid) {
            Ok(Some(file)) => return Ok(verified(path, file)),
            Ok(None) => {}
            Err(why) => refused = Some(miss(&refused, why)),
        }
    }
    if !allow_fetch {
        return Err(miss(&refused, prohibited()));
    }
    let mut download = crate::notify::Download::begin(&pin.label);
    let staged = stage(&dir, name, pin, uid, &pin.url, |staging| {
        fetch(&pin.url, staging, &mut |done, total| {
            download.progress(done, total)
        })
    });
    download.end(staged.is_ok());
    staged
        .map(|file| verified(path, file))
        .map_err(|why| miss(&refused, why))
}

/// The own-store opportunity: the stored candidate's verified handle, or
/// `None` when the name is missing or holds nothing usable, with the
/// first refusal kept in `refused`. A refused or unconvertible candidate
/// is repairable by a publication renamed over it. A component above the
/// name that cannot be traversed safely is the error: nothing may be
/// staged under it either.
fn own_copy(
    store: &Path,
    relative: &str,
    expected: &str,
    origin: &str,
    uid: u32,
    refused: &mut Option<String>,
) -> Result<Option<fs::File>, String> {
    match open_verified(store, relative, expected, origin, uid) {
        Ok(file) => Ok(Some(file)),
        Err(PinError::Missing) => Ok(None),
        Err(PinError::Final(why) | PinError::Conversion(why)) => {
            refused.get_or_insert(why);
            Ok(None)
        }
        Err(PinError::Intermediate(why)) => Err(why),
    }
}

/// The import opportunity: copy the pinned build out of the hinted store
/// `hint` into a staging scratch in `dir`, and publish it once the copy
/// itself verifies. `Ok(None)` when the hinted store does not hold the
/// name; any refusal is an unavailable opportunity the caller records.
///
/// The hint must be an absolute path without `.` or `..` components. The
/// source is pinned inside it by the confined walk, must be a regular
/// inode, and is read only through a handle for that inode, so a source
/// replaced after the pin is never read, and one mutated in place yields
/// a copy whose digest no longer matches. Nothing in the hinted store is
/// written, renamed, chmod'd or touched, and a privileged reader opens
/// the source without updating its access time.
fn import_copy(
    dir: &PinnedDir,
    hint: &Path,
    relative: &str,
    name: &str,
    pin: &Pin,
    uid: u32,
) -> Result<Option<fs::File>, String> {
    let origin = hint.join(relative).display().to_string();
    let normalized = hint.is_absolute()
        && hint
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)));
    if !normalized {
        return Err(format!(
            "{origin}: the import hint is not an absolute normalized path"
        ));
    }
    let no_atime = if uid == 0 {
        OFlags::NOATIME
    } else {
        OFlags::empty()
    };
    let source = PinnedFile::pin(hint, relative, &origin, None)
        .and_then(|pinned| pinned.readable(&origin, no_atime));
    let mut source = match source {
        Ok(file) => file,
        Err(PinError::Missing) => return Ok(None),
        Err(other) => return Err(other.message()),
    };
    #[cfg(test)]
    if let Some(race) = IMPORT_RACE.with(|r| r.borrow_mut().take()) {
        race();
    }
    stage(dir, name, pin, uid, &origin, |staging| {
        copy_bounded(&mut source, staging, FETCH_MAX_BYTES).map_err(|e| format!("{origin}: {e}"))
    })
    .map(Some)
}

#[cfg(test)]
thread_local! {
    /// Runs between opening an import source and copying from it, so a
    /// test can mutate or replace the source inside that window.
    pub(crate) static IMPORT_RACE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };

    /// Skips the in-place reopen so a test drives the procfs conversion.
    pub(crate) static FORCE_PROC_REOPEN: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// Stage a copy `fill` writes into an exclusive scratch inside the pinned
/// label directory `dir`, verify it, and publish it under `name`.
///
/// The scratch is created read-write, and its digest is computed by
/// reading back through the descriptor the bytes were written through,
/// so no name lookup separates the verified bytes from the published
/// ones. The verified bytes must also load as a container before they
/// are published. A failed fill, digest, container or publication leaves
/// nothing behind. Returns the verified handle.
fn stage(
    dir: &PinnedDir,
    name: &str,
    pin: &Pin,
    uid: u32,
    origin: &str,
    fill: impl FnOnce(&mut fs::File) -> Result<(), String>,
) -> Result<fs::File, String> {
    let scratch = format!(".{}.onelf.{}", pin.blake3, std::process::id());
    let mut staging = match dir.create_file(&scratch, staging_mode(uid)) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            // A scratch left by a process whose pid this one recycled; the
            // publication lock proves no live acquirer is staging.
            let _ = dir.unlink(&scratch);
            dir.create_file(&scratch, staging_mode(uid))
                .map_err(|e| format!("{origin}: {e}"))?
        }
        Err(e) => return Err(format!("{origin}: {e}")),
    };
    let staged = fill(&mut staging)
        .and_then(|()| check_digest(&mut staging, &pin.blake3, origin))
        .and_then(|()| {
            let copy = staging.try_clone().map_err(|e| format!("{origin}: {e}"))?;
            crate::loader::load_file(copy)
                .map(drop)
                .map_err(|e| format!("{origin}: not a usable build: {e}"))
        })
        .and_then(|()| dir.publish(&scratch, name, &staging, origin));
    if let Err(why) = staged {
        let _ = dir.unlink(&scratch);
        return Err(why);
    }
    Ok(staging)
}

/// A verified handle together with its stored path, recorded as in use.
fn verified(path: PathBuf, file: fs::File) -> VerifiedArtifact {
    let artifact = VerifiedArtifact { path, file };
    touch(&artifact.file);
    artifact
}

/// The publication lock of one label: an exclusive flock every acquirer
/// of the label's builds holds while it stages, kept in a `.{label}.lock`
/// file beside the store it guards. Read-only access suffices to hold an
/// flock, so a store shared through the hint still serializes for every
/// user that may stage there, and a first creator mints the file readable
/// to those acquirers.
fn publication_lock(store: &Path, label: &str) -> Result<fs::File, String> {
    let path = store.join(format!(".{label}.lock"));
    let lock = rustix::fs::open(
        &path,
        OFlags::CREATE | OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::RUSR | Mode::RGRP | Mode::ROTH,
    )
    .map_err(|e| format!("{}: {e}", path.display()))?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(fs::File::from(lock))
}

/// The mode a staged scratch file takes, by privilege. A privileged
/// target stages privately, so the published artifact is private too; a
/// user keeps the modes its store always carried, minus the write bit an
/// untrusted uid must never hold on a stored artifact.
fn staging_mode(uid: u32) -> Mode {
    if uid == 0 {
        Mode::RUSR | Mode::WUSR
    } else {
        Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::WGRP | Mode::ROTH
    }
}

/// Create the store root and the label directory staging writes into,
/// under the target uid's rules. A user keeps the ordinary directories
/// `create_dir_all` always produced. A privileged target requires
/// validated root-owned private directories instead: a pre-existing one
/// that is a symlink, another uid's, or open to group or other access is
/// refused (one the target owns is tightened in place), and nothing is
/// staged through it.
fn ensure_store_dirs(uid: u32, store: &Path, label_dir: &Path) -> Result<(), String> {
    if uid != 0 {
        return fs::create_dir_all(label_dir).map_err(|e| format!("{}: {e}", label_dir.display()));
    }
    for dir in [store, label_dir] {
        if !onelf_format::cache_layout::ensure_safe_dir(dir, uid) {
            return Err(format!(
                "{}: not a private directory owned by the target uid, so nothing may be staged \
                 there",
                dir.display()
            ));
        }
    }
    Ok(())
}

/// Why the confined open of a store candidate did not produce a verified
/// readable handle. Nothing was read from the candidate in any case.
#[derive(Debug)]
enum PinError {
    /// The candidate is not there: an ordinary cache miss a fetch may
    /// repair.
    Missing,
    /// The final name holds something a store build cannot be: a symlink
    /// or a non-regular object. No content was read; a fetched copy
    /// renamed over the name repairs it without opening it.
    Final(String),
    /// A component above the final name cannot be traversed safely, so
    /// nothing under it may be read or written through it.
    Intermediate(String),
    /// The pinned regular file has no readable handle for the same inode,
    /// so no handle could serve the pinned bytes. The opportunity is
    /// unavailable, and the next one is tried.
    Conversion(String),
}

impl PinError {
    /// The bounded refusal for a caller that cannot retry the lookup.
    fn message(self) -> String {
        match self {
            PinError::Missing => "the candidate went missing while it was held".to_string(),
            PinError::Final(why) | PinError::Intermediate(why) | PinError::Conversion(why) => why,
        }
    }
}

/// A store candidate pinned by descriptor: a regular file reached through
/// a walk that opened nothing for content and followed nothing inside the
/// store, held so that conversion and hashing cannot be separated from
/// the pinned inode by a rename.
struct PinnedFile {
    fd: OwnedFd,
}

/// A directory pinned by descriptor: reached through a walk that opened
/// nothing for content and followed nothing inside the store, so every
/// name under it resolves in exactly the directory the walk validated.
struct PinnedDir {
    fd: OwnedFd,
}

/// Refuse a name that cannot be one component of a stored candidate,
/// before it is looked up anywhere.
fn check_component(name: &str, origin: &str) -> Result<(), PinError> {
    if name.is_empty() || matches!(name, "." | "..") || name.contains('/') {
        return Err(PinError::Intermediate(format!(
            "{origin}: unsafe path component {name}"
        )));
    }
    Ok(())
}

/// Why a directory may not back a privileged acquirer's staging, given
/// its owner and mode against the privilege context, or `None` when `uid`
/// owns it with no group or other access.
fn insecure_dir(dir_uid: u32, mode: u32, uid: u32) -> Option<&'static str> {
    if dir_uid != uid {
        Some("is owned by another uid")
    } else if mode & 0o077 != 0 {
        Some("is accessible to group or other")
    } else {
        None
    }
}

/// Why a pinned regular file must not serve as a stored artifact, given
/// its owner and mode against the privilege context, or `None` when `uid`
/// owns it and no untrusted write access remains. A privileged context
/// refuses group write access as well; a user store keeps accepting what
/// its owner's umask produces.
fn insecure_artifact(file_uid: u32, mode: u32, uid: u32) -> Option<&'static str> {
    if file_uid != uid {
        Some("is owned by another uid")
    } else if mode & 0o002 != 0 {
        Some("is writable by other")
    } else if uid == 0 && mode & 0o020 != 0 {
        Some("is writable by group")
    } else {
        None
    }
}

impl PinnedDir {
    /// Walk from `store` down `components`, opening each as a real
    /// directory with `openat(O_PATH|O_NOFOLLOW)`, so a symlink, FIFO,
    /// device, socket or ordinary file in place of a directory stops the
    /// walk before anything under it is touched. The store root itself is
    /// opened by name with symlink following kept: it is the location the
    /// target uid owns or chose, or the hinted store an import reads.
    ///
    /// `strict` names the uid whose privilege the walk stages under, and
    /// then every walked directory must be owned by that uid with no
    /// group or other access, even when it pre-exists, so an insecure
    /// store directory cannot back a privileged acquisition.
    fn walk(
        store: &Path,
        components: &[&str],
        origin: &str,
        strict: Option<u32>,
    ) -> Result<PinnedDir, PinError> {
        let mut dir = match rustix::fs::open(
            store,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(dir) => dir,
            Err(Errno::NOENT) => return Err(PinError::Missing),
            Err(e) => return Err(PinError::Intermediate(format!("{origin}: {e}"))),
        };
        if let Some(uid) = strict {
            strict_private_dir(&dir, origin, uid)?;
        }
        for name in components {
            let (pinned, meta) = open_pinned_at(&dir, name, origin)?;
            let kind = kind_name(FileType::from_raw_mode(meta.st_mode));
            if kind != KIND_DIRECTORY {
                return Err(PinError::Intermediate(format!(
                    "{origin}: {name} is {kind}, not a directory"
                )));
            }
            if let Some(uid) = strict {
                strict_private_dir(&pinned, origin, uid)?;
            }
            dir = pinned;
        }
        Ok(PinnedDir { fd: dir })
    }

    /// Pin the candidate `name` in this directory: it must be a regular
    /// file, or the pin is refused before any content open exists. With
    /// `owner`, the candidate is a stored artifact, and its owner and
    /// permissions must leave that uid's untrusted peers no write access.
    /// Without, it is an import source whose copy the digest decides.
    fn pin_file(
        &self,
        name: &str,
        origin: &str,
        owner: Option<u32>,
    ) -> Result<PinnedFile, PinError> {
        let (pinned, meta) = open_pinned_at(&self.fd, name, origin)?;
        let kind = kind_name(FileType::from_raw_mode(meta.st_mode));
        if kind != KIND_REGULAR {
            return Err(PinError::Final(format!(
                "{origin}: {kind} at the stored name, not a regular file"
            )));
        }
        if let Some(uid) = owner
            && let Some(why) = insecure_artifact(meta.st_uid, meta.st_mode, uid)
        {
            return Err(PinError::Final(format!(
                "{origin}: the stored file {why}, so it cannot serve as the pinned build"
            )));
        }
        Ok(PinnedFile { fd: pinned })
    }

    /// Create the exclusive staging scratch `name` in this directory at
    /// `mode`, so two acquirers can never share a staging file and a
    /// scratch left by a dead process is never truncated and reused. The
    /// handle reads as well as writes, so the staged bytes are verified
    /// through the descriptor that wrote them.
    fn create_file(&self, name: &str, mode: Mode) -> io::Result<fs::File> {
        check_component(name, name).map_err(|e| io::Error::other(e.message()))?;
        Ok(fs::File::from(rustix::fs::openat(
            &self.fd,
            name,
            OFlags::CREATE | OFlags::EXCL | OFlags::RDWR | OFlags::CLOEXEC,
            mode,
        )?))
    }

    /// Remove `name` from this directory, for the staging failure paths
    /// this acquisition owns.
    fn unlink(&self, name: &str) -> io::Result<()> {
        check_component(name, name).map_err(|e| io::Error::other(e.message()))?;
        Ok(rustix::fs::unlinkat(
            &self.fd,
            name,
            rustix::fs::AtFlags::empty(),
        )?)
    }

    /// Publish the verified scratch `from` as `to` inside this directory:
    /// one atomic rename, then a check by inode that `to` now names the
    /// file `verified` holds. Rename never opens the entry it replaces, so
    /// a planted symlink or special object at `to` is swapped out without
    /// being touched. A scratch name swapped for another file before the
    /// rename publishes that other inode, which the check catches: it is
    /// removed again and the publication refused.
    fn publish(
        &self,
        from: &str,
        to: &str,
        verified: &fs::File,
        origin: &str,
    ) -> Result<(), String> {
        for name in [from, to] {
            check_component(name, origin).map_err(PinError::message)?;
        }
        rustix::fs::renameat(&self.fd, from, &self.fd, to).map_err(|e| format!("{origin}: {e}"))?;
        let ours = fstat(verified).map_err(|e| format!("{origin}: {e}"))?;
        let published = open_pinned_at(&self.fd, to, origin).map(|(_, st)| (st.st_dev, st.st_ino));
        if published.ok() != Some((ours.st_dev, ours.st_ino)) {
            let _ = self.unlink(to);
            return Err(format!(
                "{origin}: the staged copy was replaced before it was published"
            ));
        }
        Ok(())
    }
}

/// Open `name` in `dir` with `O_PATH|O_NOFOLLOW`, and return its pinned
/// descriptor together with its metadata, so every caller gates on one
/// `fstat` of the object it holds.
fn open_pinned_at(
    dir: &OwnedFd,
    name: &str,
    origin: &str,
) -> Result<(OwnedFd, rustix::fs::Stat), PinError> {
    check_component(name, origin)?;
    let pinned = match rustix::fs::openat(
        dir,
        name,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(pinned) => pinned,
        Err(Errno::NOENT) => return Err(PinError::Missing),
        Err(e) => {
            return Err(PinError::Intermediate(format!("{origin}: {name}: {e}")));
        }
    };
    let meta =
        fstat(&pinned).map_err(|e| PinError::Intermediate(format!("{origin}: {name}: {e}")))?;
    Ok((pinned, meta))
}

/// Validate a pinned store directory against the privileged rules, so an
/// insecure or user-controlled pre-existing store path never backs a
/// privileged acquisition.
fn strict_private_dir(dir: &OwnedFd, origin: &str, uid: u32) -> Result<(), PinError> {
    let meta = fstat(dir).map_err(|e| PinError::Intermediate(format!("{origin}: {e}")))?;
    if let Some(why) = insecure_dir(meta.st_uid, meta.st_mode, uid) {
        return Err(PinError::Intermediate(format!(
            "{origin}: the store directory {why}, so it cannot back a privileged acquisition"
        )));
    }
    Ok(())
}

impl PinnedFile {
    /// Pin the store-relative candidate `relative` under `store`.
    ///
    /// The store root is opened by name with symlink following kept: it is
    /// the location the target uid owns or chose. Every component inside
    /// it is opened with `openat(O_PATH|O_NOFOLLOW)`, so a symlink, FIFO,
    /// device, socket or ordinary file in place of a directory stops the
    /// walk before anything under it is touched, and a symlink or
    /// non-regular object at the final name is refused by the `fstat`
    /// file-type check without being opened for I/O. With `owner`, the
    /// final name must also be owned by that uid with no untrusted write
    /// access, or it is refused as a stored artifact.
    fn pin(
        store: &Path,
        relative: &str,
        origin: &str,
        owner: Option<u32>,
    ) -> Result<PinnedFile, PinError> {
        let names: Vec<&str> = relative.split('/').collect();
        if names.len() > 8 {
            return Err(PinError::Intermediate(format!(
                "{origin}: too many path components"
            )));
        }
        let (components, name) = names.split_at(names.len() - 1);
        let dir = PinnedDir::walk(store, components, origin, None)?;
        dir.pin_file(name[0], origin, owner)
    }

    /// Pin a source named by an absolute path whose final component must
    /// not be a symlink, for a `file://` fetch: the directories above it
    /// resolve as the path names them, and the final object must be a
    /// regular file by the `fstat` of its pinned descriptor.
    fn pin_path(path: &str, origin: &str) -> Result<PinnedFile, PinError> {
        let fd = match rustix::fs::open(
            path,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Err(PinError::Missing),
            Err(e) => return Err(PinError::Intermediate(format!("{origin}: {e}"))),
        };
        let meta = fstat(&fd).map_err(|e| PinError::Intermediate(format!("{origin}: {e}")))?;
        let kind = kind_name(FileType::from_raw_mode(meta.st_mode));
        if kind != KIND_REGULAR {
            return Err(PinError::Final(format!(
                "{origin}: {kind}, not a regular file"
            )));
        }
        Ok(PinnedFile { fd })
    }

    /// Convert the pinned descriptor into a readable [`fs::File`] for the
    /// same inode, without any lookup through the store. `extra` adds
    /// open flags, such as `O_NOATIME` for a privileged import.
    ///
    /// [`reopen_in_place`] makes the conversion. The reopened descriptor
    /// is accepted only when `fstat` reports the pinned device and inode,
    /// so nothing but the pinned regular file can be served. Where no
    /// mechanism works, or the pinned file cannot be reopened, the
    /// conversion is refused; the untrusted path is never reopened by
    /// name.
    fn readable(self, origin: &str, extra: OFlags) -> Result<fs::File, PinError> {
        let PinnedFile { fd } = self;
        let pinned = fstat(&fd).map_err(|e| PinError::Conversion(format!("{origin}: {e}")))?;
        let reopened = reopen_in_place(&fd, extra).map_err(|e| {
            PinError::Conversion(format!(
                "{origin}: the pinned file cannot be reopened in place: {e}"
            ))
        })?;
        let got = fstat(&reopened).map_err(|e| PinError::Conversion(format!("{origin}: {e}")))?;
        if (got.st_dev, got.st_ino) != (pinned.st_dev, pinned.st_ino) {
            return Err(PinError::Conversion(format!(
                "{origin}: the reopened handle does not match the pinned inode"
            )));
        }
        Ok(fs::File::from(reopened))
    }
}

/// The [`kind_name`] of the only file type a store candidate may be.
const KIND_REGULAR: &str = "a regular file";

/// The [`kind_name`] of the only file type a walk may descend into.
const KIND_DIRECTORY: &str = "a directory";

/// The bounded name of a file type, for refusals that say what was found
/// instead of what was wanted.
fn kind_name(kind: FileType) -> &'static str {
    match kind {
        FileType::RegularFile => KIND_REGULAR,
        FileType::Directory => KIND_DIRECTORY,
        FileType::Symlink => "a symlink",
        FileType::Fifo => "a FIFO",
        FileType::Socket => "a socket",
        FileType::CharacterDevice => "a character device",
        FileType::BlockDevice => "a block device",
        FileType::Unknown => "an unknown object",
    }
}

/// The Linux `O_EMPTY_PATH` open bit, which makes `openat` with an empty
/// path operate on `dirfd` itself. Linux-specific and absent from
/// `rustix`'s flag set, so it is spelled by value.
#[cfg(target_os = "linux")]
const EMPTY_PATH: OFlags = OFlags::from_bits_retain(0x0400_0000);

/// Reopen `pinned` for reading without a lookup through the store. The
/// first attempt is `openat` with an empty path and `O_EMPTY_PATH`, which
/// reopens the descriptor itself. A kernel that does not know the bit
/// ignores it and fails the empty path, so the conversion falls back to
/// the descriptor's `/proc/self/fd` entry, opened through a `/proc`
/// directory descriptor whose `fstatfs` proves it is procfs. The caller
/// checks the result against the pinned inode either way.
#[cfg(target_os = "linux")]
fn reopen_in_place(pinned: &OwnedFd, extra: OFlags) -> io::Result<OwnedFd> {
    #[cfg(test)]
    let skip = FORCE_PROC_REOPEN.with(std::cell::Cell::get);
    #[cfg(not(test))]
    let skip = false;
    if !skip
        && let Ok(fd) = rustix::fs::openat(
            pinned,
            "",
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | EMPTY_PATH | extra,
            Mode::empty(),
        )
    {
        return Ok(fd);
    }
    let proc = rustix::fs::open(
        "/proc",
        OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    if rustix::fs::fstatfs(&proc)?.f_type != rustix::fs::PROC_SUPER_MAGIC {
        return Err(io::Error::other("/proc is not a procfs mount"));
    }
    Ok(rustix::fs::openat(
        &proc,
        format!("self/fd/{}", pinned.as_raw_fd()),
        OFlags::RDONLY | OFlags::CLOEXEC | extra,
        Mode::empty(),
    )?)
}

/// A platform without the Linux reopen mechanisms has no safe conversion,
/// so every conversion is refused.
#[cfg(not(target_os = "linux"))]
fn reopen_in_place(_pinned: &OwnedFd, _extra: OFlags) -> io::Result<OwnedFd> {
    Err(io::Error::other(
        "no same-inode reopen mechanism on this platform",
    ))
}

/// Open the store candidate `relative` under `store` through the confined
/// walk, convert its pinned inode to a readable handle, and hash the
/// handle's bytes against `expected`. Returns that handle, so every
/// consumer reads the bytes whose digest matched.
fn open_verified(
    store: &Path,
    relative: &str,
    expected: &str,
    origin: &str,
    uid: u32,
) -> Result<fs::File, PinError> {
    let mut file =
        PinnedFile::pin(store, relative, origin, Some(uid))?.readable(origin, OFlags::empty())?;
    check_digest(&mut file, expected, origin).map_err(PinError::Final)?;
    Ok(file)
}

/// Hash everything `file` holds from its start against `expected`. The
/// refusal names the pin and not the digest found, so a mismatching
/// file a privileged reader could open discloses nothing about its
/// contents.
fn check_digest(file: &mut fs::File, expected: &str, origin: &str) -> Result<(), String> {
    file.rewind().map_err(|e| format!("{origin}: {e}"))?;
    let got = hash_stream(file).map_err(|e| format!("{origin}: {e}"))?;
    if got != expected {
        return Err(format!("{origin}: hash mismatch (pinned {expected})"));
    }
    Ok(())
}

/// The BLAKE3 digest of everything readable from `file`, in lowercase
/// hex. Fixed-size reads keep memory bounded whatever the file's size.
fn hash_stream(file: &fs::File) -> io::Result<String> {
    let mut file = file;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// The largest fetch a store stages: two gibibytes, an order of magnitude
/// above the largest builds a GL stack or a dependency set produces, so
/// the bound never refuses a real build while it still stops a hostile
/// source, a growing file or a never-ending stream long before it can
/// fill the store's disk. The container codecs keep their own bounds on
/// declared sizes; this caps the file the store has to hold.
const FETCH_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Copy or download the build into the already-created exclusive staging
/// file. The scheme decides: a local file is pinned as a regular file
/// whose final component is no symlink and copied through the bounded
/// staging copy, `https://` streams through the embedding caller's client
/// into the same descriptor, and anything else is refused before a
/// request is made. Either scheme is bounded by [`FETCH_MAX_BYTES`]
/// during the transfer, so a source that never ends cannot fill the
/// store's disk.
fn fetch(
    url: &str,
    staging: &mut fs::File,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<(), String> {
    if let Some(path) = url.strip_prefix("file://") {
        let mut source = PinnedFile::pin_path(path, url)
            .and_then(|pinned| pinned.readable(url, OFlags::empty()))
            .map_err(|e| match e {
                PinError::Missing => format!("{url}: no such file"),
                other => other.message(),
            })?;
        if let Ok(meta) = source.metadata()
            && meta.len() > FETCH_MAX_BYTES
        {
            return Err(format!(
                "{url}: the source is larger than the {FETCH_MAX_BYTES}-byte bound a staged \
                 build may take"
            ));
        }
        return copy_bounded(&mut source, staging, FETCH_MAX_BYTES)
            .map_err(|e| format!("{url}: {e}"));
    }
    if url.starts_with("https://") {
        let Some(download) = HTTPS_FETCH.get() else {
            return Err(format!("this runtime has no HTTPS client to fetch {url}"));
        };
        download(url, staging, FETCH_MAX_BYTES, progress)?;
        let len = staging.metadata().map_err(|e| e.to_string())?.len();
        if len > FETCH_MAX_BYTES {
            return Err(format!(
                "{url}: the source is larger than the {FETCH_MAX_BYTES}-byte bound a staged \
                 build may take"
            ));
        }
        return Ok(());
    }
    Err(format!("{url}: a build must be an https:// or file:// URL"))
}

/// Stream `source` into the staging file from its start, refusing more
/// than `cap` bytes so a hostile source cannot fill the disk.
fn copy_bounded(
    source: &mut impl io::Read,
    staging: &mut fs::File,
    cap: u64,
) -> Result<(), String> {
    let copied = io::copy(&mut source.take(cap.saturating_add(1)), staging)
        .map_err(|e| format!("the staged copy failed: {e}"))?;
    if copied > cap {
        return Err(format!(
            "the source is larger than the {cap}-byte bound a staged build may take"
        ));
    }
    Ok(())
}

/// Record a use through the verified handle, so collection can tell an
/// idle build from one in use.
fn touch(file: &fs::File) {
    let _ = file.set_modified(std::time::SystemTime::now());
}

#[cfg(test)]
mod tests {
    use super::*;

    use onelf_format::{
        Block, Entry, EntryKind, EntryPoint, Flags, Footer, Manifest, ManifestHeader,
    };

    /// The one file a test container carries, and its stored bytes.
    const FILE_NAME: &str = "lib/libdemo.so";
    const FILE_CONTENT: &[u8] = b"demo set library bytes\n";

    /// A minimal stored-mode container the runtime loader can open,
    /// together with the digest that names it in a store.
    struct Container {
        bytes: Vec<u8>,
        digest: String,
    }

    /// Build one: `[prefix][manifest][stored payload][footer]`, one
    /// verbatim block for [`FILE_CONTENT`].
    fn container() -> Container {
        let prefix = b"onelf platform fixture";
        let mut string_table: Vec<u8> = vec![0];
        let name_offset = string_table.len() as u32;
        string_table.extend_from_slice(FILE_NAME.as_bytes());
        string_table.push(0);

        let hash = *blake3::hash(FILE_CONTENT).as_bytes();
        let entry = Entry {
            kind: EntryKind::File,
            parent: u32::MAX,
            name: name_offset,
            mode: 0o755,
            mtime_secs: 0,
            mtime_nsec: 0,
            content_hash: hash,
            blocks: vec![Block {
                payload_offset: 0,
                compressed_size: FILE_CONTENT.len() as u64,
                original_size: FILE_CONTENT.len() as u64,
                content_hash: hash,
            }],
            symlink_target: 0,
        };
        let manifest = Manifest {
            header: ManifestHeader {
                version: 3,
                entry_count: 1,
                string_table_size: string_table.len() as u32,
                entrypoint_count: 0,
                default_entrypoint: 0,
                lib_dir_count: 0,
                name_offset: 0,
                package_id: [0; 32],
                prefix_hash: *blake3::hash(prefix).as_bytes(),
                footer_hash: [0; 32],
            },
            entrypoints: Vec::<EntryPoint>::new(),
            entries: vec![entry],
            lib_dir_offsets: Vec::new(),
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
            payload_size: FILE_CONTENT.len() as u64,
            dict_offset: 0,
            dict_size: 0,
            manifest_checksum: onelf_format::manifest::manifest_checksum(&raw).unwrap(),
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(prefix);
        bytes.extend_from_slice(&compressed);
        bytes.extend_from_slice(FILE_CONTENT);
        footer.write_to(&mut bytes).unwrap();
        let digest = blake3::hash(&bytes).to_hex().to_string();
        Container { bytes, digest }
    }

    #[test]
    fn the_sweep_removes_only_idle_builds() {
        let store = temp_dir("sweep");
        let day = std::time::Duration::from_secs(86400);
        let now = std::time::SystemTime::now();
        let put = |relative: &str, used: std::time::SystemTime| {
            let path = store.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let file = fs::File::create(&path).unwrap();
            file.set_len(10).unwrap();
            file.set_modified(used).unwrap();
            path
        };
        let hash = |c: char| c.to_string().repeat(64);
        let old = put(&format!("gl/{}.onelf", hash('a')), now - 40 * day);
        let fresh = put(&format!("gl/{}.onelf", hash('b')), now - day);
        let lone = put(&format!("qt/{}.onelf", hash('c')), now - 40 * day);
        let partial = put(&format!("gl/.{}.onelf", hash('d')), now - 40 * day);
        let ahead = put(&format!("gl/{}.onelf", hash('e')), now + day);
        let package = put("apps/editor.onelf", now - 40 * day);

        assert_eq!(sweep_idle_builds(&store, 30 * day, false), (2, 20));
        assert!(!old.exists() && !lone.exists());
        assert!(
            store.join("qt").exists(),
            "a launch's sweep keeps the emptied label"
        );
        assert!(fresh.exists() && partial.exists() && ahead.exists());
        assert!(
            package.exists(),
            "a file not named by a digest is not a build"
        );

        put(&format!("qt/{}.onelf", hash('f')), now - 40 * day);
        assert_eq!(sweep_idle_builds(&store, 30 * day, true), (1, 10));
        assert!(!store.join("qt").exists(), "the emptied label goes");
        let _ = fs::remove_dir_all(&store);
    }

    /// A private store directory for one test.
    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("onelf-platform-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The uid this process really runs as: the privilege context of the
    /// user-side fixtures, against which the store's ownership rules
    /// decide.
    fn me() -> u32 {
        crate::paths::uid()
    }

    /// Where the store keeps a build with `digest` inside `dir`.
    fn stored_in(dir: &Path, digest: &str) -> PathBuf {
        dir.join("set").join(format!("{digest}.onelf"))
    }

    /// The store-relative candidate name of a build with `digest`, under
    /// the label every test uses.
    fn stored_rel(digest: &str) -> String {
        format!("set/{digest}.onelf")
    }

    /// The pin whose cached build lives at `digest`, with `source` as the
    /// configured fetch source.
    fn pin_for(digest: &str, source: &Path) -> Pin {
        Pin {
            label: "set".into(),
            url: format!("file://{}", source.display()),
            blake3: digest.into(),
        }
    }

    /// Load the container again through a duplicate of the held handle.
    fn reload(artifact: &VerifiedArtifact) -> crate::loader::PackageData {
        crate::loader::load_file(artifact.file.try_clone().unwrap()).unwrap()
    }

    /// The verified payload of [`FILE_NAME`] as the loader reads it from
    /// `pkg`.
    fn entry_bytes(pkg: &crate::loader::PackageData) -> Vec<u8> {
        let idx = (0..pkg.manifest.entries.len())
            .find(|&i| pkg.manifest.entry_path(i) == FILE_NAME)
            .expect("the container carries the file");
        crate::loader::read_verified_entry(
            &pkg.file,
            &pkg.footer,
            &pkg.manifest.entries[idx],
            &pkg.codec,
        )
        .unwrap()
    }

    #[test]
    fn a_record_parses_and_unescapes() {
        let hash = "AB".repeat(32);
        let pin = parse_pin(&format!(
            "label = \"platform-1\"\nurl = \"https://e/a\\\"b\"\nblake3 = \"{hash}\"\n"
        ))
        .unwrap();
        assert_eq!(pin.label, "platform-1");
        assert_eq!(pin.url, "https://e/a\"b");
        assert_eq!(pin.blake3, "ab".repeat(32));
        assert!(parse_pin("label = \"x\"\n").is_none(), "incomplete");
        assert!(
            parse_pin(&format!(
                "label = \"../x\"\nurl = \"u\"\nblake3 = \"{hash}\"\n"
            ))
            .is_none(),
            "a label is a directory name"
        );
        assert!(
            parse_pin("label = \"x\"\nurl = \"u\"\nblake3 = \"abc\"\n").is_none(),
            "the hash names a file and is checked against bytes"
        );
    }

    #[test]
    fn a_mismatch_or_bad_scheme_leaves_nothing_behind() {
        let dir = temp_dir("mismatch");
        let source = dir.join("source");
        fs::write(&source, b"not the pinned bytes").unwrap();
        let pin = Pin {
            label: "l".into(),
            url: format!("file://{}", source.display()),
            blake3: "0".repeat(64),
        };
        let tmp_relative = format!("l/.{}.onelf.{}", pin.blake3, std::process::id());
        let tmp = dir.join(&tmp_relative);
        fs::create_dir_all(tmp.parent().unwrap()).unwrap();
        let mut staging = fs::File::create(&tmp).unwrap();
        let err = fetch(&pin.url, &mut staging, &mut |_, _| {})
            .and_then(|()| {
                open_verified(&dir, &tmp_relative, &pin.blake3, &pin.url, me())
                    .map_err(PinError::message)
            })
            .unwrap_err();
        assert!(err.contains("hash mismatch"), "{err}");
        let err = fetch("http://e/gl.onelf", &mut staging, &mut |_, _| {}).unwrap_err();
        assert!(err.contains("https:// or file://"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bitflipped_cached_file_is_rejected_before_any_load() {
        let dir = temp_dir("bitflip");
        let c = container();
        let stored = stored_in(&dir, &c.digest);
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        let mut bytes = c.bytes.clone();
        bytes[5] ^= 0xff;
        fs::write(&stored, &bytes).unwrap();

        let pin = pin_for(&c.digest, &dir.join("absent-source"));
        let err = acquire(&dir, &stored_rel(&c.digest), &pin, false, me(), None).unwrap_err();
        assert!(err.contains("hash mismatch"), "{err}");
        assert!(
            !err.contains("not a usable build"),
            "the loader must not run on unverified bytes: {err}"
        );
        assert_eq!(fs::read(&stored).unwrap(), bytes, "the store is left alone");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_truncated_cached_container_is_rejected_before_any_load() {
        let dir = temp_dir("truncated");
        let c = container();
        let stored = stored_in(&dir, &c.digest);
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        let bytes = &c.bytes[..c.bytes.len() - 40];
        fs::write(&stored, bytes).unwrap();

        let pin = pin_for(&c.digest, &dir.join("absent-source"));
        let err = acquire(&dir, &stored_rel(&c.digest), &pin, false, me(), None).unwrap_err();
        assert!(err.contains("hash mismatch"), "{err}");
        assert!(
            !err.contains("not a usable build") && !err.contains("too small"),
            "the loader must not run on unverified bytes: {err}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_verified_file_flows_to_consumers_through_its_handle() {
        let dir = temp_dir("handle");
        let c = container();
        let stored = stored_in(&dir, &c.digest);
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        fs::write(&stored, &c.bytes).unwrap();

        let pin = pin_for(&c.digest, &dir.join("absent-source"));
        let artifact = acquire(&dir, &stored_rel(&c.digest), &pin, false, me(), None).unwrap();
        assert_eq!(artifact.path(), stored.as_path());
        assert_eq!(entry_bytes(&reload(&artifact)), FILE_CONTENT);

        // The name is swapped for other bytes and then removed; the held
        // inode is what every consumer keeps reading.
        let swapped = dir.join("swapped");
        fs::write(&swapped, b"swapped in place").unwrap();
        fs::rename(&swapped, &stored).unwrap();
        assert_eq!(entry_bytes(&reload(&artifact)), FILE_CONTENT);
        fs::remove_file(&stored).unwrap();
        assert_eq!(entry_bytes(&reload(&artifact)), FILE_CONTENT);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_store_file_is_refetched_and_replaced() {
        let dir = temp_dir("refetch");
        let c = container();
        let stored = stored_in(&dir, &c.digest);
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        fs::write(&stored, b"damaged in place").unwrap();
        let source = dir.join("source");
        fs::write(&source, &c.bytes).unwrap();

        let pin = pin_for(&c.digest, &source);
        let artifact = acquire(&dir, &stored_rel(&c.digest), &pin, true, me(), None).unwrap();
        assert_eq!(fs::read(&stored).unwrap(), c.bytes, "the store is repaired");
        assert_eq!(entry_bytes(&reload(&artifact)), FILE_CONTENT);
        assert_eq!(
            fs::read_dir(stored.parent().unwrap()).unwrap().count(),
            1,
            "no fetch scratch is left behind"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_refetch_reports_the_integrity_failure() {
        let dir = temp_dir("refetch-fails");
        let c = container();
        let stored = stored_in(&dir, &c.digest);
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        fs::write(&stored, b"damaged in place").unwrap();
        let source = dir.join("source");
        fs::write(&source, b"not the pinned bytes either").unwrap();

        let pin = pin_for(&c.digest, &source);
        let err = acquire(&dir, &stored_rel(&c.digest), &pin, true, me(), None).unwrap_err();
        assert!(err.contains("hash mismatch"), "{err}");
        assert_eq!(
            fs::read_dir(stored.parent().unwrap()).unwrap().count(),
            1,
            "the rejected fetch leaves only the stored file"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_store_file_with_fetch_disabled_reports_the_mismatch() {
        let dir = temp_dir("empty");
        let stored = stored_in(&dir, &"0".repeat(64));
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        fs::write(&stored, b"").unwrap();

        let pin = pin_for(&"0".repeat(64), &dir.join("absent-source"));
        let err = acquire(&dir, &stored_rel(&"0".repeat(64)), &pin, false, me(), None).unwrap_err();
        assert!(err.contains("hash mismatch"), "{err}");
        assert!(err.contains("ONELF_NO_PLATFORM_FETCH"), "{err}");

        let missing = stored_in(&dir, &"1".repeat(64));
        assert!(!missing.exists(), "the second entry is absent");
        let err = acquire(
            &dir,
            &stored_rel(&"1".repeat(64)),
            &pin_for(&"1".repeat(64), &dir.join("absent")),
            false,
            me(),
            None,
        )
        .unwrap_err();
        assert!(err.contains("fetching is disabled"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A symlink at the stored name is refused without reading through
    /// it, even when its target carries exactly the pinned bytes: the
    /// refusal is a property of the path, not of a digest. With fetching
    /// allowed, a fetched copy renamed over the link repairs the store.
    #[test]
    fn a_symlink_at_the_stored_name_is_refused_and_replaced() {
        let dir = temp_dir("symlink-final");
        let c = container();
        let stored = stored_in(&dir, &c.digest);
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        let target = dir.join("elsewhere");
        fs::write(&target, &c.bytes).unwrap();
        std::os::unix::fs::symlink(&target, &stored).unwrap();

        let pin = pin_for(&c.digest, &dir.join("absent-source"));
        let err = acquire(&dir, &stored_rel(&c.digest), &pin, false, me(), None).unwrap_err();
        assert!(err.contains("symlink"), "{err}");
        assert!(err.contains("not a regular file"), "{err}");
        assert!(err.contains("ONELF_NO_PLATFORM_FETCH"), "{err}");
        assert_eq!(
            fs::read_link(&stored).unwrap(),
            target,
            "the refusal leaves the link alone"
        );

        let source = dir.join("source");
        fs::write(&source, &c.bytes).unwrap();
        let artifact = acquire(
            &dir,
            &stored_rel(&c.digest),
            &pin_for(&c.digest, &source),
            true,
            me(),
            None,
        )
        .unwrap();
        assert!(!stored.is_symlink(), "the fetched copy is a real file");
        assert_eq!(fs::read(&stored).unwrap(), c.bytes);
        assert_eq!(entry_bytes(&reload(&artifact)), FILE_CONTENT);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A symlinked label directory is refused outright: nothing under it
    /// is read even when the bytes behind the link match the pin, and no
    /// fetch is published through the link. The refusal stands with
    /// fetching allowed.
    #[test]
    fn a_symlinked_label_directory_is_refused_without_traversal() {
        let dir = temp_dir("symlink-label");
        let c = container();
        let evil = dir.join("evil");
        fs::create_dir_all(&evil).unwrap();
        fs::write(evil.join(format!("{}.onelf", c.digest)), &c.bytes).unwrap();
        std::os::unix::fs::symlink(&evil, dir.join("set")).unwrap();
        let source = dir.join("source");
        fs::write(&source, &c.bytes).unwrap();

        let pin = pin_for(&c.digest, &source);
        for allow_fetch in [false, true] {
            let err =
                acquire(&dir, &stored_rel(&c.digest), &pin, allow_fetch, me(), None).unwrap_err();
            assert!(err.contains("symlink"), "{err}");
            assert_eq!(
                fs::read_dir(&evil).unwrap().count(),
                1,
                "nothing is read or written through the link"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// Special objects planted at the stored name, and at the label
    /// directory, are refused by the file-type check on the pinned
    /// descriptor before any content open exists. A FIFO is the proof: an
    /// open for reading would have blocked forever or read nothing, and
    /// either way the error would speak of a read, not of the file type.
    /// Devices share the one file-type gate these fixtures exercise with
    /// a FIFO, a socket and a directory.
    #[test]
    fn special_objects_are_refused_before_any_content_io() {
        use rustix::fs::mkfifoat;

        for tag in ["fifo", "socket", "dir"] {
            let dir = temp_dir(&format!("special-{tag}"));
            // A shortened name keeps the planted path inside a unix
            // socket's address limit; the refusal happens long before the
            // digest is consulted.
            let digest = "0".repeat(24);
            let label = dir.join("set");
            fs::create_dir_all(&label).unwrap();
            let at = stored_in(&dir, &digest);
            match tag {
                "fifo" => {
                    let handle = fs::File::open(&label).unwrap();
                    mkfifoat(&handle, format!("{digest}.onelf"), Mode::RUSR | Mode::WUSR).unwrap();
                }
                "socket" => {
                    let _listener = std::os::unix::net::UnixListener::bind(&at).unwrap();
                }
                _ => {
                    fs::create_dir(&at).unwrap();
                }
            }
            let pin = pin_for(&digest, &dir.join("absent-source"));
            let err = acquire(&dir, &stored_rel(&digest), &pin, false, me(), None).unwrap_err();
            assert!(err.contains("not a regular file"), "{tag}: {err}");
            assert!(
                !err.contains("hash mismatch"),
                "{tag}: no byte may have been read: {err}"
            );
            let _ = fs::remove_dir_all(&dir);
        }

        // A FIFO at the label directory stops the walk before the stored
        // name is even reached.
        let dir = temp_dir("special-fifo-label");
        let digest = "0".repeat(64);
        let root = fs::File::open(&dir).unwrap();
        mkfifoat(&root, "set", Mode::RUSR | Mode::WUSR).unwrap();
        drop(root);
        let pin = pin_for(&digest, &dir.join("absent-source"));
        let err = acquire(&dir, &stored_rel(&digest), &pin, false, me(), None).unwrap_err();
        assert!(err.contains("not a directory"), "{err}");
        assert!(
            !err.contains("hash mismatch"),
            "no byte may have been read: {err}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The converted handle reads the pinned inode, not the stored name:
    /// bytes renamed in while the descriptor is pinned never reach the
    /// verification or the consumer.
    #[test]
    fn the_converted_handle_reads_the_pinned_inode_not_the_name() {
        let dir = temp_dir("conversion-swap");
        let c = container();
        let stored = stored_in(&dir, &c.digest);
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        fs::write(&stored, &c.bytes).unwrap();

        let pinned = PinnedFile::pin(
            &dir,
            &stored_rel(&c.digest),
            &stored.display().to_string(),
            Some(me()),
        )
        .unwrap();
        let hostile = dir.join("hostile");
        fs::write(&hostile, b"hostile replacement").unwrap();
        fs::rename(&hostile, &stored).unwrap();

        let file = pinned
            .readable(&stored.display().to_string(), OFlags::empty())
            .map_err(PinError::message)
            .unwrap();
        let pkg = crate::loader::load_file(file).unwrap();
        assert_eq!(entry_bytes(&pkg), FILE_CONTENT);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A pinned regular file that cannot be reopened in place is refused
    /// with a bounded error, and nothing reopens the stored path by name:
    /// the readable hostile bytes renamed in at that name would otherwise
    /// have been hashed. Here the refusal comes from the file's own
    /// permissions; on a kernel without the Linux reopen mechanism the
    /// same refusal comes from the missing flag instead.
    #[test]
    fn an_unreadable_pinned_file_is_refused_without_reopening_by_name() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("conversion-refused");
        let c = container();
        let stored = stored_in(&dir, &c.digest);
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        fs::write(&stored, &c.bytes).unwrap();
        let mut perms = fs::metadata(&stored).unwrap().permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&stored, perms).unwrap();

        let pinned = PinnedFile::pin(
            &dir,
            &stored_rel(&c.digest),
            &stored.display().to_string(),
            Some(me()),
        )
        .unwrap();
        let hostile = dir.join("hostile");
        fs::write(&hostile, b"hostile replacement").unwrap();
        fs::rename(&hostile, &stored).unwrap();

        match pinned.readable(&stored.display().to_string(), OFlags::empty()) {
            Ok(_) => assert_eq!(
                crate::paths::uid(),
                0,
                "only a uid that may read the file converts it"
            ),
            Err(PinError::Conversion(why)) => {
                assert!(
                    why.contains("reopened in place") || why.contains("does not match"),
                    "{why}"
                );
            }
            Err(why) => panic!(
                "the refusal has the wrong class: {}",
                PinError::message(why)
            ),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// The store root is reached by name on purpose: it is the location
    /// the target uid owns or chose, and a symlink there resolves as the
    /// user arranged it. Everything inside it is confined.
    #[test]
    fn a_symlinked_store_root_is_followed_but_nothing_inside_it() {
        let dir = temp_dir("symlink-root");
        let c = container();
        let real = dir.join("real");
        let stored = stored_in(&real, &c.digest);
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        fs::write(&stored, &c.bytes).unwrap();
        let link = dir.join("store");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let pin = pin_for(&c.digest, &dir.join("absent-source"));
        let artifact = acquire(&link, &stored_rel(&c.digest), &pin, false, me(), None).unwrap();
        assert_eq!(artifact.path(), link.join(stored_rel(&c.digest)));
        assert_eq!(entry_bytes(&reload(&artifact)), FILE_CONTENT);
        let _ = fs::remove_dir_all(&dir);
    }

    /// An ordinary uid's relocation to a store it owns, or to one that
    /// does not exist yet, resolves exactly as given, and no candidate
    /// base is even consulted for it.
    #[test]
    fn an_ordinary_uid_keeps_the_relocated_store() {
        if me() == 0 {
            return;
        }
        let dir = temp_dir("relocate");
        for hint in [dir.clone(), dir.join("not-yet")] {
            let store = owned_store(
                me(),
                Some(hint.as_path()),
                || unreachable!("the cache base is not consulted"),
                || unreachable!("the private base is not consulted"),
            )
            .unwrap();
            assert_eq!(store, hint);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// A hint naming a store another uid owns, as `sudo -u other -E`
    /// leaves behind, is never the target's write destination: the
    /// target's own cache serves, and the hint stays import input only.
    #[test]
    fn an_ordinary_uid_refuses_another_uids_store_as_its_own() {
        if me() == 0 {
            return;
        }
        let base = temp_dir("foreign-hint");
        let store = owned_store(
            me(),
            Some(Path::new("/")),
            || Some(base.clone()),
            || unreachable!("the private base is not consulted"),
        )
        .unwrap();
        assert_eq!(store, base.join("platform"));
        let _ = fs::remove_dir_all(&base);
    }

    /// A hinted store that does not exist yet is the uid's own only when
    /// its nearest existing ancestor is.
    #[test]
    fn a_missing_hint_is_judged_by_its_nearest_existing_ancestor() {
        if me() == 0 {
            return;
        }
        let base = temp_dir("missing-hint");
        fs::create_dir_all(&base).unwrap();
        let fresh = base.join("not/yet/created");
        assert!(store_owned_by(&fresh, me()));
        assert!(!store_owned_by(
            Path::new("/onelf-missing-hint/store"),
            me()
        ));
        let store = owned_store(
            me(),
            Some(Path::new("/onelf-missing-hint/store")),
            || Some(base.clone()),
            || unreachable!("the private base is not consulted"),
        )
        .unwrap();
        assert_eq!(store, base.join("platform"));
        let _ = fs::remove_dir_all(&base);
    }

    /// Without a hint, and with an empty one, the per-user cache serves.
    #[test]
    fn an_ordinary_uid_without_a_hint_uses_the_cache_base() {
        let base = temp_dir("cachebase");
        for hint in [None, Some(Path::new(""))] {
            let store = owned_store(
                1000,
                hint,
                || Some(base.clone()),
                || unreachable!("the private base is not consulted"),
            )
            .unwrap();
            assert_eq!(store, base.join("platform"));
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn an_ordinary_uid_without_a_cache_base_is_an_error() {
        let err = owned_store(1000, None, || None, || unreachable!()).unwrap_err();
        assert!(err.contains("no cache directory"), "{err}");
    }

    /// Root writes nowhere the environment names. The inherited hint and
    /// the HOME/XDG-derived cache root were both chosen by the
    /// unprivileged caller; only the validated root-owned private base
    /// serves, and without one the decision fails closed. The refusals
    /// hold for a hint naming a user-owned store and for no hint at all.
    #[test]
    fn root_refuses_every_env_derived_store() {
        let hinted = Path::new("/home/attacker/.cache/onelf/platform");
        let store = owned_store(
            0,
            Some(hinted),
            || Some(PathBuf::from("/home/attacker/.cache/onelf")),
            || Some(PathBuf::from("/tmp/onelf-0")),
        )
        .unwrap();
        assert_eq!(store, PathBuf::from("/tmp/onelf-0/platform"));

        let store = owned_store(
            0,
            None,
            || Some(PathBuf::from("/home/attacker/.cache/onelf")),
            || Some(PathBuf::from("/tmp/onelf-0")),
        )
        .unwrap();
        assert_eq!(store, PathBuf::from("/tmp/onelf-0/platform"));

        let err = owned_store(0, Some(hinted), || None, || None).unwrap_err();
        assert!(err.contains("no root-owned private directory"), "{err}");
    }

    /// Refusing the cache root means never reading it: no stat, no
    /// creation, nothing under a path the environment chose.
    #[test]
    fn root_never_consults_the_cache_base() {
        let store = owned_store(
            0,
            None,
            || panic!("the HOME/XDG-derived cache root was consulted"),
            || Some(PathBuf::from("/tmp/onelf-0")),
        )
        .unwrap();
        assert_eq!(store, PathBuf::from("/tmp/onelf-0/platform"));
    }

    /// Two acquirers of one build serialize on the label's publication
    /// lock: the loser waits out the winner's staging, re-verifies the
    /// published bytes against the pin, and uses them instead of fetching
    /// its own copy, so exactly one publication wins. The source cannot
    /// itself produce the pinned bytes, so the loser's success proves it
    /// reused the winner's publication rather than fetching.
    #[test]
    fn concurrent_acquirers_serialize_and_the_loser_reuses_the_publication() {
        use rustix::fs::FlockOperation;

        let _locking = crate::test_locks::locking();
        let dir = temp_dir("stage-serialize");
        let c = container();
        let store = dir.join("store");
        let stored = stored_in(&store, &c.digest);
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        let source = dir.join("source");
        fs::write(&source, b"never the pinned bytes").unwrap();
        let pin = pin_for(&c.digest, &source);
        let lock_path = store.join(".set.lock");
        let held = fs::File::create(&lock_path).unwrap();
        rustix::fs::flock(&held, FlockOperation::LockExclusive).unwrap();
        let winner = {
            let stored = stored.clone();
            let bytes = c.bytes.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(150));
                fs::write(&stored, &bytes).unwrap();
                drop(held);
            })
        };
        let artifact = acquire(&store, &stored_rel(&c.digest), &pin, true, me(), None).unwrap();
        winner.join().unwrap();
        assert_eq!(artifact.path(), stored.as_path());
        assert_eq!(entry_bytes(&reload(&artifact)), FILE_CONTENT);
        assert_eq!(
            fs::read_dir(stored.parent().unwrap()).unwrap().count(),
            1,
            "exactly one publication, and none of the loser's staging"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// An interrupted copy stages truncated bytes, which the destination
    /// verification refuses before anything is published, and the failed
    /// staging is cleaned away: no incomplete file is ever served under
    /// the stored name.
    #[test]
    fn an_interrupted_copy_is_discarded_and_never_published() {
        let _locking = crate::test_locks::locking();
        let dir = temp_dir("stage-interrupted");
        let c = container();
        let store = dir.join("store");
        let source = dir.join("source");
        fs::write(&source, &c.bytes[..c.bytes.len() - 40]).unwrap();
        let pin = pin_for(&c.digest, &source);
        let err = acquire(&store, &stored_rel(&c.digest), &pin, true, me(), None).unwrap_err();
        assert!(err.contains("hash mismatch"), "{err}");
        assert!(
            !stored_in(&store, &c.digest).exists(),
            "nothing incomplete was published"
        );
        let label = store.join("set");
        assert_eq!(
            fs::read_dir(&label).unwrap().count(),
            0,
            "the failed staging is cleaned away"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The staged bytes are verified against the pin, through the
    /// descriptor that wrote them, before publication: staging that does
    /// not match is refused with nothing published and nothing left
    /// behind, as is a hash-matching staging that is no container, while
    /// staging that matches is published and loads as the pinned build.
    #[test]
    fn a_mismatched_staging_is_never_published() {
        use std::io::Write;

        let dir = temp_dir("stage-mismatch");
        let c = container();
        let store = dir.join("store");
        fs::create_dir_all(store.join("set")).unwrap();
        let label = PinnedDir::walk(&store, &["set"], "staging", None).unwrap();
        let name = format!("{}.onelf", c.digest);
        let pin = pin_for(&c.digest, &dir.join("unused"));

        let err = stage(&label, &name, &pin, me(), "staging", |f| {
            f.write_all(b"bytes that match nothing")
                .map_err(|e| e.to_string())
        })
        .unwrap_err();
        assert!(err.contains("hash mismatch"), "{err}");
        assert!(
            !err.contains(
                &blake3::hash(b"bytes that match nothing")
                    .to_hex()
                    .to_string()
            )
        );
        assert_eq!(fs::read_dir(store.join("set")).unwrap().count(), 0);

        let junk = b"hash-matching bytes that are no container";
        let junk_pin = pin_for(&blake3::hash(junk).to_hex(), &dir.join("unused"));
        let err = stage(&label, "junk.onelf", &junk_pin, me(), "staging", |f| {
            f.write_all(junk).map_err(|e| e.to_string())
        })
        .unwrap_err();
        assert!(err.contains("not a usable build"), "{err}");
        assert_eq!(fs::read_dir(store.join("set")).unwrap().count(), 0);

        let file = stage(&label, &name, &pin, me(), "staging", |f| {
            f.write_all(&c.bytes).map_err(|e| e.to_string())
        })
        .unwrap();
        assert_eq!(fs::read(stored_in(&store, &c.digest)).unwrap(), c.bytes);
        let pkg = crate::loader::load_file(file).unwrap();
        assert_eq!(entry_bytes(&pkg), FILE_CONTENT);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A scratch name swapped for another file between staging and the
    /// rename is caught by the inode check after publication: the
    /// foreign file is removed again and nothing stays published.
    #[test]
    fn a_swapped_scratch_name_is_never_published() {
        let dir = temp_dir("stage-swap");
        let c = container();
        let store = dir.join("store");
        fs::create_dir_all(store.join("set")).unwrap();
        let label = PinnedDir::walk(&store, &["set"], "staging", None).unwrap();
        let mut verified = label.create_file(".scratch", staging_mode(me())).unwrap();
        std::io::Write::write_all(&mut verified, &c.bytes).unwrap();
        fs::write(store.join("set/.other"), &c.bytes).unwrap();
        fs::rename(store.join("set/.other"), store.join("set/.scratch")).unwrap();

        let err = label
            .publish(".scratch", "final.onelf", &verified, "staging")
            .unwrap_err();
        assert!(err.contains("replaced before it was published"), "{err}");
        assert_eq!(fs::read_dir(store.join("set")).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A pre-existing stored file an untrusted uid could rewrite is not
    /// usable: the world-writable file is refused without being touched,
    /// and a permitted fetch repairs it by renaming a verified staging
    /// over it, which never opens what it replaces.
    #[test]
    fn an_insecure_preexisting_destination_is_refused_and_repaired() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let _locking = crate::test_locks::locking();
        let dir = temp_dir("stage-insecure");
        let c = container();
        let store = dir.join("store");
        let stored = stored_in(&store, &c.digest);
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        fs::write(&stored, &c.bytes).unwrap();
        fs::set_permissions(&stored, PermissionsExt::from_mode(0o666)).unwrap();

        let pin = pin_for(&c.digest, &dir.join("absent-source"));
        let err = acquire(&store, &stored_rel(&c.digest), &pin, false, me(), None).unwrap_err();
        assert!(err.contains("writable by other"), "{err}");
        assert_eq!(
            fs::symlink_metadata(&stored).unwrap().mode() & 0o777,
            0o666,
            "the refusal leaves the file alone"
        );

        let source = dir.join("source");
        fs::write(&source, &c.bytes).unwrap();
        let artifact = acquire(
            &store,
            &stored_rel(&c.digest),
            &pin_for(&c.digest, &source),
            true,
            me(),
            None,
        )
        .unwrap();
        assert_eq!(fs::read(&stored).unwrap(), c.bytes);
        let mode = fs::symlink_metadata(&stored).unwrap().mode() & 0o777;
        assert_eq!(mode & 0o002, 0, "the repair publishes no other write");
        // The repaired mode is what the staging always carries under this
        // process's umask, so a probe scratch created the same way is the
        // reference.
        let dir_pin = PinnedDir::walk(&store, &["set"], "probe", None).unwrap();
        drop(
            dir_pin
                .create_file(".probe.onelf.1", staging_mode(me()))
                .unwrap(),
        );
        let probe = fs::symlink_metadata(store.join("set/.probe.onelf.1"))
            .unwrap()
            .mode()
            & 0o777;
        assert_eq!(mode, probe, "the repaired mode is the staging mode");
        assert_eq!(entry_bytes(&reload(&artifact)), FILE_CONTENT);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A stored file owned by another uid is refused when the acquisition
    /// runs with another uid's privilege: ownership, not the bytes,
    /// decides, so a user-owned file planted under a privileged target's
    /// stored name is a cache miss, never a usable artifact.
    #[test]
    fn a_foreign_owned_stored_file_is_refused_for_a_privileged_context() {
        let _locking = crate::test_locks::locking();
        let dir = temp_dir("stage-foreign");
        let c = container();
        let store = dir.join("store");
        let stored = stored_in(&store, &c.digest);
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        fs::write(&stored, &c.bytes).unwrap();

        let pin = pin_for(&c.digest, &dir.join("absent-source"));
        let err = acquire(&store, &stored_rel(&c.digest), &pin, false, 0, None).unwrap_err();
        assert!(err.contains("owned by another uid"), "{err}");
        assert_eq!(fs::read(&stored).unwrap(), c.bytes, "the refusal is quiet");

        // The same file serves the uid that owns it: the gate compares
        // the privilege context, not the bytes' provenance.
        let artifact = acquire(&store, &stored_rel(&c.digest), &pin, false, me(), None).unwrap();
        assert_eq!(entry_bytes(&reload(&artifact)), FILE_CONTENT);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The ownership and permission gates decide by the same rules the
    /// committed private-directory checks use: no other uid's files, no
    /// write access for an untrusted uid, and, for a privileged context,
    /// no group write either, since a directory group member is not
    /// root's to trust.
    #[test]
    fn the_security_gates_decide_by_owner_and_write_bits() {
        assert_eq!(insecure_artifact(1000, 0o644, 1000), None);
        assert_eq!(
            insecure_artifact(1000, 0o644, 0),
            Some("is owned by another uid")
        );
        assert_eq!(
            insecure_artifact(1000, 0o666, 1000),
            Some("is writable by other")
        );
        assert_eq!(insecure_artifact(1000, 0o664, 1000), None);
        assert_eq!(insecure_artifact(0, 0o664, 0), Some("is writable by group"));
        assert_eq!(insecure_artifact(0, 0o600, 0), None);

        assert_eq!(insecure_dir(1000, 0o700, 1000), None);
        assert_eq!(
            insecure_dir(1000, 0o700, 0),
            Some("is owned by another uid")
        );
        assert_eq!(
            insecure_dir(1000, 0o755, 1000),
            Some("is accessible to group or other")
        );
        assert_eq!(insecure_dir(0, 0o700, 0), None);
    }

    /// A user-owned store directory cannot back a privileged acquirer's
    /// staging: the directories are validated against the privilege
    /// context before anything is staged, and the refusal tightens
    /// nothing and writes nothing.
    #[test]
    fn a_user_owned_store_is_refused_for_a_privileged_staging() {
        let _locking = crate::test_locks::locking();
        let dir = temp_dir("stage-rootrefuse");
        let store = dir.join("store");
        let label = store.join("set");
        fs::create_dir_all(&label).unwrap();

        let err = ensure_store_dirs(0, &store, &label).unwrap_err();
        assert!(
            err.contains("not a private directory owned by the target uid"),
            "{err}"
        );
        assert!(label.is_dir(), "the refusal leaves the directory alone");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A source over the staging bound is refused, whether it declares
    /// its size or grows past the bound while it is copied, so a hostile
    /// source cannot fill the disk.
    #[test]
    fn an_oversized_source_is_refused_by_the_copy_bound() {
        let dir = temp_dir("stage-bound");
        let mut staging = fs::File::create(dir.join("staging")).unwrap();
        let mut source = &b"0123456789abcdef0123456789abcdef"[..];
        let err = copy_bounded(&mut source, &mut staging, 16).unwrap_err();
        assert!(err.contains("larger than the 16-byte bound"), "{err}");

        let mut staging = fs::File::create(dir.join("staging-2")).unwrap();
        let mut source = &b"0123456789abcdef"[..];
        copy_bounded(&mut source, &mut staging, 16).unwrap();
        drop(staging);
        assert_eq!(
            fs::read(dir.join("staging-2")).unwrap().len(),
            16,
            "the bounded copy stages exactly what the source held"
        );

        // A source that grows while it is copied is cut off at the same
        // bound: the fifo has no declared size, so only the bound during
        // the copy decides.
        let fifo = dir.join("growing");
        rustix::fs::mkfifoat(
            fs::File::open(&dir).unwrap(),
            "growing",
            Mode::RUSR | Mode::WUSR,
        )
        .unwrap();
        let writer = {
            let fifo = fifo.clone();
            std::thread::spawn(move || {
                let mut sink = fs::File::create(&fifo).unwrap();
                use std::io::Write;
                sink.write_all(&[7u8; 64]).unwrap();
            })
        };
        let mut source = fs::File::open(&fifo).unwrap();
        let mut staging = fs::File::create(dir.join("staging-3")).unwrap();
        let err = copy_bounded(&mut source, &mut staging, 16).unwrap_err();
        assert!(err.contains("larger than the 16-byte bound"), "{err}");
        writer.join().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    /// The staging mode follows the privilege seam: a privileged target
    /// stages privately, and a user stages with the write access its own
    /// store always carried, minus the other-write bit no stored artifact
    /// may keep.
    #[test]
    fn the_staging_mode_follows_the_privilege_seam() {
        assert_eq!(staging_mode(0).as_raw_mode(), 0o600);
        assert_eq!(staging_mode(1000).as_raw_mode(), 0o664);
        assert_eq!(staging_mode(me()).as_raw_mode() & 0o002, 0);
    }

    /// A scratch left under this acquisition's own pid name by a process
    /// whose pid was recycled is reclaimed: the exclusive creation
    /// removes it and stages fresh, so a dead acquirer's partial bytes
    /// are never reused.
    #[test]
    fn a_stale_scratch_from_a_recycled_pid_is_reclaimed() {
        let _locking = crate::test_locks::locking();
        let dir = temp_dir("stage-stale");
        let c = container();
        let store = dir.join("store");
        let source = dir.join("source");
        fs::write(&source, &c.bytes).unwrap();
        let pin = pin_for(&c.digest, &source);
        let scratch = format!("set/.{}.onelf.{}", pin.blake3, std::process::id());
        fs::create_dir_all(store.join("set")).unwrap();
        fs::write(store.join(&scratch), b"left by a dead process").unwrap();

        let artifact = acquire(&store, &stored_rel(&c.digest), &pin, true, me(), None).unwrap();
        let stored = stored_in(&store, &c.digest);
        assert_eq!(fs::read(&stored).unwrap(), c.bytes);
        assert_eq!(entry_bytes(&reload(&artifact)), FILE_CONTENT);
        assert_eq!(
            fs::read_dir(stored.parent().unwrap()).unwrap().count(),
            1,
            "the scratch became the published artifact, nothing else remains"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The bytes, mode, owner and modification time of every entry under
    /// `dir`, so a store can be compared before and after an import.
    fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>, u32, u32, i64, i64)> {
        use std::os::unix::fs::MetadataExt;

        let mut out = Vec::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(path) = pending.pop() {
            let md = fs::symlink_metadata(&path).unwrap();
            let bytes = if md.is_file() {
                fs::read(&path).unwrap()
            } else {
                Vec::new()
            };
            if md.is_dir() {
                pending.extend(fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
            }
            out.push((
                path,
                bytes,
                md.mode(),
                md.uid(),
                md.mtime(),
                md.mtime_nsec(),
            ));
        }
        out.sort();
        out
    }

    /// A user store holding the container under the test label, and the
    /// own store an acquirer publishes into, side by side under `dir`.
    fn import_fixture(dir: &Path, c: &Container) -> (PathBuf, PathBuf) {
        let user_store = dir.join("user-store");
        let source = stored_in(&user_store, &c.digest);
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, &c.bytes).unwrap();
        (user_store, dir.join("own-store"))
    }

    /// With fetching disabled and the pin's source unable to serve the
    /// build, an artifact in the hinted store is imported: copied into
    /// the own store, verified there, published, and served from the
    /// own copy, while the hinted store stays byte, mode and mtime
    /// identical.
    #[test]
    fn an_import_from_the_hinted_store_needs_no_fetch() {
        let _locking = crate::test_locks::locking();
        let dir = temp_dir("import");
        let c = container();
        let (user_store, own) = import_fixture(&dir, &c);
        let before = snapshot(&user_store);

        let pin = pin_for(&c.digest, &dir.join("absent-source"));
        let artifact = acquire(
            &own,
            &stored_rel(&c.digest),
            &pin,
            false,
            me(),
            Some(&user_store),
        )
        .unwrap();
        assert_eq!(artifact.path(), stored_in(&own, &c.digest));
        assert_eq!(fs::read(stored_in(&own, &c.digest)).unwrap(), c.bytes);
        assert_eq!(entry_bytes(&reload(&artifact)), FILE_CONTENT);
        assert_eq!(snapshot(&user_store), before, "the hinted store moved");

        // The published copy now serves as the own-store artifact, with
        // the hinted store gone entirely.
        fs::remove_dir_all(&user_store).unwrap();
        let again = acquire(&own, &stored_rel(&c.digest), &pin, false, me(), None).unwrap();
        assert_eq!(entry_bytes(&reload(&again)), FILE_CONTENT);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A source changed in place after it was pinned yields a copy whose
    /// own digest no longer matches: the import is refused, nothing is
    /// published, no scratch remains, and with fetching disabled the
    /// refusal ends the acquisition naming both reasons.
    #[test]
    fn a_source_mutated_during_import_is_never_published() {
        let _locking = crate::test_locks::locking();
        let dir = temp_dir("import-mutate");
        let c = container();
        let (user_store, own) = import_fixture(&dir, &c);
        let source = stored_in(&user_store, &c.digest);
        let mutated = source.clone();
        IMPORT_RACE.with(|r| {
            *r.borrow_mut() = Some(Box::new(move || {
                use std::os::unix::fs::FileExt;
                let file = fs::OpenOptions::new().write(true).open(&mutated).unwrap();
                file.write_all_at(b"X", 5).unwrap();
            }))
        });

        let pin = pin_for(&c.digest, &dir.join("absent-source"));
        let err = acquire(
            &own,
            &stored_rel(&c.digest),
            &pin,
            false,
            me(),
            Some(&user_store),
        )
        .unwrap_err();
        assert!(err.contains("hash mismatch"), "{err}");
        assert!(err.contains("fetching is disabled"), "{err}");
        assert!(
            !stored_in(&own, &c.digest).exists(),
            "nothing was published"
        );
        assert_eq!(fs::read_dir(own.join("set")).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A source replaced by rename after it was pinned is never read: the
    /// copy comes from the pinned inode, so what is published is exactly
    /// the pinned bytes, and the hostile replacement is never consumed.
    #[test]
    fn a_source_replaced_during_import_publishes_only_the_pinned_bytes() {
        let _locking = crate::test_locks::locking();
        let dir = temp_dir("import-replace");
        let c = container();
        let (user_store, own) = import_fixture(&dir, &c);
        let source = stored_in(&user_store, &c.digest);
        let hostile = dir.join("hostile");
        fs::write(&hostile, b"hostile replacement").unwrap();
        let (from, to) = (hostile.clone(), source.clone());
        IMPORT_RACE
            .with(|r| *r.borrow_mut() = Some(Box::new(move || fs::rename(&from, &to).unwrap())));

        let pin = pin_for(&c.digest, &dir.join("absent-source"));
        let artifact = acquire(
            &own,
            &stored_rel(&c.digest),
            &pin,
            false,
            me(),
            Some(&user_store),
        )
        .unwrap();
        assert_eq!(fs::read(&source).unwrap(), b"hostile replacement");
        assert_eq!(fs::read(stored_in(&own, &c.digest)).unwrap(), c.bytes);
        assert_eq!(entry_bytes(&reload(&artifact)), FILE_CONTENT);
        let _ = fs::remove_dir_all(&dir);
    }

    /// An unsafe import source is an unavailable opportunity, never a
    /// fatal error and never a read through the unsafe object: a symlink
    /// at the source name, a FIFO there, and a relative hint all fall
    /// through to the permitted fetch, and with fetching disabled the
    /// acquisition fails naming the refusal and the prohibition.
    #[test]
    fn an_unsafe_import_source_falls_through_to_the_fetch() {
        let _locking = crate::test_locks::locking();
        for tag in ["symlink", "fifo", "relative"] {
            let dir = temp_dir(&format!("import-unsafe-{tag}"));
            let c = container();
            let (user_store, own) = import_fixture(&dir, &c);
            let source = stored_in(&user_store, &c.digest);
            let elsewhere = dir.join("elsewhere");
            fs::write(&elsewhere, &c.bytes).unwrap();
            let mut hint = user_store.clone();
            match tag {
                "symlink" => {
                    fs::remove_file(&source).unwrap();
                    std::os::unix::fs::symlink(&elsewhere, &source).unwrap();
                }
                "fifo" => {
                    fs::remove_file(&source).unwrap();
                    let label = fs::File::open(source.parent().unwrap()).unwrap();
                    rustix::fs::mkfifoat(
                        &label,
                        source.file_name().unwrap(),
                        Mode::RUSR | Mode::WUSR,
                    )
                    .unwrap();
                }
                _ => hint = PathBuf::from("relative/store"),
            }
            let rel = stored_rel(&c.digest);

            let pin = pin_for(&c.digest, &dir.join("absent-source"));
            let err = acquire(&own, &rel, &pin, false, me(), Some(&hint)).unwrap_err();
            assert!(err.contains("fetching is disabled"), "{tag}: {err}");
            let refusal = match tag {
                "symlink" => "a symlink",
                "fifo" => "a FIFO",
                _ => "not an absolute normalized path",
            };
            assert!(err.contains(refusal), "{tag}: {err}");

            let pin = pin_for(&c.digest, &elsewhere);
            let artifact = acquire(&own, &rel, &pin, true, me(), Some(&hint)).unwrap();
            assert_eq!(entry_bytes(&reload(&artifact)), FILE_CONTENT);
            let _ = fs::remove_dir_all(&dir);
        }
    }

    /// The procfs conversion serves the pinned inode exactly as the
    /// in-place reopen does: with the in-place attempt skipped, a pinned
    /// file whose name was swapped still converts to the pinned bytes,
    /// and both paths agree with the pin on device and inode.
    #[test]
    fn the_procfs_conversion_serves_the_pinned_inode() {
        use std::os::unix::fs::MetadataExt;

        let dir = temp_dir("conversion-proc");
        let c = container();
        let stored = stored_in(&dir, &c.digest);
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        fs::write(&stored, &c.bytes).unwrap();
        let origin = stored.display().to_string();
        let ino = fs::metadata(&stored).unwrap().ino();

        for force in [false, true] {
            let pinned =
                PinnedFile::pin(&dir, &stored_rel(&c.digest), &origin, Some(me())).unwrap();
            let held = dir.join("held");
            fs::rename(&stored, &held).unwrap();
            fs::write(&stored, b"hostile replacement").unwrap();
            FORCE_PROC_REOPEN.with(|f| f.set(force));
            let file = pinned.readable(&origin, OFlags::empty());
            FORCE_PROC_REOPEN.with(|f| f.set(false));
            let file = file.map_err(PinError::message).unwrap();
            assert_eq!(file.metadata().unwrap().ino(), ino, "force={force}");
            let pkg = crate::loader::load_file(file).unwrap();
            assert_eq!(entry_bytes(&pkg), FILE_CONTENT, "force={force}");
            fs::rename(&held, &stored).unwrap();
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// A stored candidate that cannot be converted to a readable handle
    /// is an unavailable opportunity, not an error: the permitted fetch
    /// replaces it by rename and serves the verified copy. On a uid that
    /// may read anything the candidate converts and serves directly.
    #[test]
    fn an_unconvertible_own_copy_falls_through_to_the_fetch() {
        use std::os::unix::fs::PermissionsExt;

        let _locking = crate::test_locks::locking();
        let dir = temp_dir("conversion-fallthrough");
        let c = container();
        let store = dir.join("store");
        let stored = stored_in(&store, &c.digest);
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        fs::write(&stored, &c.bytes).unwrap();
        fs::set_permissions(&stored, PermissionsExt::from_mode(0o000)).unwrap();
        let source = dir.join("source");
        fs::write(&source, &c.bytes).unwrap();

        let pin = pin_for(&c.digest, &source);
        let artifact = acquire(&store, &stored_rel(&c.digest), &pin, true, me(), None).unwrap();
        assert_eq!(entry_bytes(&reload(&artifact)), FILE_CONTENT);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A `file://` source is opened without following a symlink at its
    /// final component and must be a regular file, so a planted link to
    /// a root-readable file or a FIFO is refused before any read.
    #[test]
    fn a_file_source_must_be_a_regular_file_reached_without_a_final_symlink() {
        let dir = temp_dir("file-source");
        let target = dir.join("target");
        fs::write(&target, b"some root-readable bytes").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        rustix::fs::mkfifoat(
            fs::File::open(&dir).unwrap(),
            "fifo",
            Mode::RUSR | Mode::WUSR,
        )
        .unwrap();
        let mut staging = fs::File::create(dir.join("staging")).unwrap();

        for (name, kind) in [("link", "a symlink"), ("fifo", "a FIFO")] {
            let url = format!("file://{}", dir.join(name).display());
            let err = fetch(&url, &mut staging, &mut |_, _| {}).unwrap_err();
            assert!(
                err.contains(kind) && err.contains("not a regular file"),
                "{err}"
            );
        }
        assert_eq!(staging.metadata().unwrap().len(), 0, "nothing was read");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The inherited hint never becomes a privileged target's staging
    /// ground: its store resolves to the private base alone, and whether
    /// the privileged acquisition imports, verifies, or fetches, the
    /// user store's contents, mode and mtime never move. Here the private
    /// base is not root's, so every privileged opportunity is refused.
    #[test]
    fn root_stages_nowhere_but_its_own_store() {
        let _locking = crate::test_locks::locking();
        let dir = temp_dir("stage-user-store");
        let c = container();
        let (user_store, _) = import_fixture(&dir, &c);
        let before = snapshot(&user_store);

        let resolved = owned_store(
            0,
            Some(user_store.as_path()),
            || Some(dir.join("cache")),
            || Some(dir.join("private")),
        )
        .unwrap();
        assert_eq!(resolved, dir.join("private").join("platform"));

        let pin = pin_for(&c.digest, &dir.join("absent"));
        let err = acquire(&resolved, &stored_rel(&c.digest), &pin, false, 0, None).unwrap_err();
        assert!(err.contains("fetching is disabled"), "{err}");

        let source = dir.join("source");
        fs::write(&source, &c.bytes).unwrap();
        for allow_fetch in [false, true] {
            let outcome = acquire(
                &resolved,
                &stored_rel(&c.digest),
                &pin_for(&c.digest, &source),
                allow_fetch,
                0,
                Some(&user_store),
            );
            if me() != 0 {
                assert!(
                    outcome.is_err(),
                    "a user-owned private base is refused as a privileged store"
                );
            }
        }
        assert_eq!(snapshot(&user_store), before, "the user store moved");
        let _ = fs::remove_dir_all(&dir);
    }
}

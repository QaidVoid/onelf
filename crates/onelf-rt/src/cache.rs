//! Cache-based extraction with content-addressable storage.
//!
//! Extracts package contents to `~/.cache/onelf/pkg/{package_id}/` using a CAS
//! (content-addressable store) for file deduplication. Files are stored by their
//! BLAKE3 hash and hardlinked into the package directory.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use onelf_format::{EntryKind, symlink_target_within_root};

use crate::loader::{self, PackageData};
// Opens the per-package lock without CLOEXEC, so the shared lock taken by
// `ensure_extracted` survives the exec into the target and keeps a concurrent
// process's GC from deleting a package that is still in use.
use crate::paths::open_lock_inheritable;

use onelf_format::cache_layout as layout;

/// Monotonic counter making CAS temp-file names unique within a process;
/// combined with the pid it is unique across concurrent extractions. The
/// view staging borrows it for the same purpose.
pub(crate) static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Returns true if the file at `path` hashes to `expected`. Reads the
/// file incrementally so verifying a large CAS entry uses bounded memory.
/// Used to decide whether an existing CAS entry can be trusted for reuse
/// or has been poisoned/corrupted and must be re-extracted.
fn file_hashes_to(path: &Path, expected: &[u8; 32]) -> bool {
    let Ok(mut f) = fs::File::open(path) else {
        return false;
    };
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(_) => return false,
        };
    }
    hasher.finalize().as_bytes() == expected
}

/// Resolve the persistent cache root, or `None` when no safe location can
/// be established, in which case the caller refuses rather than extracting
/// into an untrusted path. The rules live in `onelf_format::cache_layout` so
/// the packer CLI applies exactly the same ones: root takes its private
/// base and never an inherited `HOME` or `XDG_CACHE_HOME`.
fn cache_dir() -> Option<PathBuf> {
    onelf_format::cache_layout::resolve_root(
        rustix::process::getuid().as_raw(),
        crate::paths::private_dir(),
    )
}

/// The lowercase hex of `bytes`: the form package ids, CAS shard names,
/// and view identity keys take on disk.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Extract all package contents directly into `target_dir` without CAS.
/// Used for ephemeral tmpfs-backed extraction inside a private namespace,
/// where dedup has no value and the whole tree is thrown away on exit.
pub(crate) fn extract_direct(pkg: &mut PackageData, target_dir: &Path) -> io::Result<()> {
    extract_matching(pkg, target_dir, |_| true)
}

/// [`extract_direct`] for the entries whose path `keep` accepts. A kept
/// entry's parent directories are created whether or not they are kept.
pub fn extract_matching(
    pkg: &mut PackageData,
    target_dir: &Path,
    keep: impl Fn(&Path) -> bool,
) -> io::Result<()> {
    let manifest = &pkg.manifest;

    // Dirs first
    for (i, entry) in manifest.entries.iter().enumerate() {
        if entry.kind == EntryKind::Dir {
            let rel = manifest.validated_entry_path(i)?;
            if rel.as_os_str().is_empty() || !keep(&rel) {
                continue;
            }
            fs::create_dir_all(target_dir.join(&rel))?;
        }
    }

    // Files (before symlinks, so no file is written through a symlink).
    for i in onelf_format::payload::file_order(manifest, &pkg.codec) {
        let entry = &manifest.entries[i];
        let rel = manifest.validated_entry_path(i)?;
        if !keep(&rel) {
            continue;
        }
        let out_path = target_dir.join(&rel);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let data = loader::read_verified_entry(&pkg.file, &pkg.footer, entry, &pkg.codec)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", rel.display())))?;

        // Never through whatever already sits there: a symlink a launch
        // put over a bundled library would carry the write to the host
        // file it names.
        if out_path.symlink_metadata().is_ok() {
            fs::remove_file(&out_path)?;
        }
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&out_path)?;
        f.write_all(&data)?;
        f.set_permissions(fs::Permissions::from_mode(entry.mode & 0o777))?;
    }

    // Symlinks last; refuse any target that escapes the root.
    for (i, entry) in manifest.entries.iter().enumerate() {
        if entry.kind != EntryKind::Symlink {
            continue;
        }
        let rel = manifest.validated_entry_path(i)?;
        if !keep(&rel) {
            continue;
        }
        let target = manifest.get_string(entry.symlink_target);
        if !symlink_target_within_root(&rel, target) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "onelf: symlink target escapes package root",
            ));
        }
        let link_path = target_dir.join(&rel);

        if let Some(parent) = link_path.parent() {
            fs::create_dir_all(parent)?;
        }
        if link_path.symlink_metadata().is_ok() {
            fs::remove_file(&link_path)?;
        }
        std::os::unix::fs::symlink(target, &link_path)?;
    }

    Ok(())
}

/// Extract `pkg` into the cache and return its directory together with a
/// shared lock guard. The guard must be kept alive through exec: while any
/// process holds it, GC cannot acquire the exclusive lock and therefore
/// cannot remove the package.
pub fn ensure_extracted(pkg: &mut PackageData) -> io::Result<(PathBuf, fs::File)> {
    use rustix::fs::FlockOperation;

    let base = cache_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "onelf: no safe cache directory (set HOME or XDG_RUNTIME_DIR)",
        )
    })?;
    let package_id = hex(&pkg.manifest.header.package_id);
    let pkg_parent = base.join("pkg");
    let pkg_dir = pkg_parent.join(&package_id);
    let cas_dir = base.join("cas");
    let lock_dir = base.join("lock");
    let meta_dir = base.join("meta");
    // Completion marker, written only after a full extraction and rename.
    // pkg_dir existing is not proof of completeness: an older onelf release
    // extracted in place, so an interrupted run could leave a partial
    // pkg_dir behind. Gating cache hits on the marker forces such trees to
    // be re-extracted instead of executed.
    let ready_path = layout::ready_marker(&base, &package_id);
    let is_complete = || pkg_dir.exists() && ready_path.exists();

    // Take the shared "in-use" lock *before* checking for the package.
    // Locking first closes the race where a concurrent GC removes pkg_dir
    // between the existence check and the lock: while we hold the shared
    // lock, GC cannot acquire the exclusive lock it needs to remove the
    // package. The fd is inheritable so the lock survives exec.
    fs::create_dir_all(&lock_dir)?;
    let lock_path = layout::lock_path(&base, &package_id);
    let lock_file = open_lock_inheritable(&lock_path)?;
    rustix::fs::flock(&lock_file, FlockOperation::LockShared)
        .map_err(|e| io::Error::other(format!("flock: {e}")))?;

    // Fast path: fully extracted (marker present) and pinned by our lock.
    if is_complete() {
        touch_meta(&meta_dir, &package_id);
        return Ok((pkg_dir, lock_file));
    }

    // Not extracted yet. Serialize extraction on a *separate* mutex so a
    // second runner waits only for extraction to finish, not for the first
    // instance's whole lifetime: the shared in-use lock is held through
    // exec, so reusing it as the extraction lock would block later waiters
    // until the extractor exits.
    let extract_path = layout::extract_lock_path(&base, &package_id);
    let extract_lock = fs::File::create(&extract_path)?;
    rustix::fs::flock(&extract_lock, FlockOperation::LockExclusive)
        .map_err(|e| io::Error::other(format!("flock: {e}")))?;

    // Another runner may have completed extraction while we waited on the
    // mutex. We still hold the shared in-use lock, so a freshly extracted
    // dir cannot be GC'd from under us.
    if !is_complete() {
        fs::create_dir_all(&cas_dir)?;
        fs::create_dir_all(&pkg_parent)?;

        // Clear any stale or partial pkg_dir (an interrupted run, or an
        // in-place tree from an older release) so the atomic rename below
        // has a clean target and never merges onto leftover files.
        let _ = fs::remove_dir_all(&pkg_dir);
        let _ = fs::remove_file(&ready_path);

        // Extract into a per-package temp dir, then atomically rename it to
        // pkg_dir. A stale temp from a crashed extraction is removed first,
        // and any failure removes the temp so the next run re-extracts
        // rather than reusing a partial tree.
        let tmp_dir = layout::extract_tmp_dir(&base, &package_id);
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir)?;

        let cas_lock = open_lock_inheritable(&layout::cas_lock_path(&base))?;
        rustix::fs::flock(&cas_lock, FlockOperation::LockShared)
            .map_err(|e| io::Error::other(format!("flock: {e}")))?;

        if let Err(e) = extract_to_cas(pkg, &cas_dir, &tmp_dir) {
            let _ = fs::remove_dir_all(&tmp_dir);
            return Err(e);
        }
        drop(cas_lock);
        if let Err(e) = fs::rename(&tmp_dir, &pkg_dir) {
            let _ = fs::remove_dir_all(&tmp_dir);
            return Err(e);
        }

        // Publish the completion marker only once the tree is fully in
        // place. If this fails the tree would be re-extracted every run, so
        // treat it as an extraction failure and roll back.
        if let Err(e) = fs::File::create(&ready_path) {
            let _ = fs::remove_dir_all(&pkg_dir);
            return Err(e);
        }
    }

    touch_meta(&meta_dir, &package_id);

    // Release the extraction mutex; the shared in-use lock stays held
    // (through exec) for the lifetime of this instance.
    drop(extract_lock);
    Ok((pkg_dir, lock_file))
}

fn extract_to_cas(pkg: &mut PackageData, cas_dir: &Path, pkg_dir: &Path) -> io::Result<()> {
    let manifest = &pkg.manifest;

    // First pass: create directories
    for (i, entry) in manifest.entries.iter().enumerate() {
        if entry.kind == EntryKind::Dir {
            let rel = manifest.validated_entry_path(i)?;
            if rel.as_os_str().is_empty() {
                continue;
            }
            fs::create_dir_all(pkg_dir.join(&rel))?;
        }
    }

    // Second pass: extract files to CAS and create hardlinks
    for i in onelf_format::payload::file_order(manifest, &pkg.codec) {
        let entry = &manifest.entries[i];

        // Validate the link path before any I/O so a hostile name is
        // rejected before it can create directories.
        let rel = manifest.validated_entry_path(i)?;

        let hash_hex = hex(&entry.content_hash);
        let shard = &hash_hex[..2];
        let cas_shard_dir = cas_dir.join(shard);
        // Keyed by content and mode: a blob is one inode hardlinked into
        // every package tree that carries it, so two packages shipping the
        // same bytes as 0644 and 0755 cannot share one.
        let mode = entry.mode & 0o777;
        let cas_path = cas_shard_dir.join(format!("{hash_hex}-{mode:03o}"));

        // Reuse an existing CAS entry only if its bytes actually hash to
        // the requested value. A slot whose content does not verify was
        // poisoned or corrupted (the CAS is shared across packages), so
        // re-extract over it instead of trusting it.
        let reuse = cas_path.exists() && file_hashes_to(&cas_path, &entry.content_hash);
        if !reuse {
            fs::create_dir_all(&cas_shard_dir)?;

            // read_verified_entry hashes the decompressed bytes against
            // entry.content_hash, so the CAS filename (derived from that
            // hash) is only ever populated with verified content.
            let data = loader::read_verified_entry(&pkg.file, &pkg.footer, entry, &pkg.codec)?;

            // Atomic write: unique temp file then rename. A per-process
            // pid+sequence name avoids two concurrent extractions of the
            // same content hash (the CAS is shared across packages)
            // clobbering each other's in-progress temp file.
            let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let tmp_path =
                cas_shard_dir.join(format!(".{hash_hex}.{}.{seq}.tmp", std::process::id()));
            let write = (|| -> io::Result<()> {
                let mut f = fs::File::create(&tmp_path)?;
                f.write_all(&data)?;
                f.set_permissions(fs::Permissions::from_mode(mode))?;
                Ok(())
            })();
            if let Err(e) = write {
                let _ = fs::remove_file(&tmp_path);
                return Err(e);
            }
            fs::rename(&tmp_path, &cas_path)?;
        }

        // Hardlink into pkg dir (avoids readlink issues with symlinks)
        let link_path = pkg_dir.join(&rel);

        if let Some(parent) = link_path.parent() {
            fs::create_dir_all(parent)?;
        }

        if link_path.symlink_metadata().is_ok() {
            fs::remove_file(&link_path)?;
        }
        fs::hard_link(&cas_path, &link_path)?;
    }

    // Third pass: create symlinks last; refuse targets escaping the root.
    for (i, entry) in manifest.entries.iter().enumerate() {
        if entry.kind != EntryKind::Symlink {
            continue;
        }

        let rel = manifest.validated_entry_path(i)?;
        let target = manifest.get_string(entry.symlink_target);
        if !symlink_target_within_root(&rel, target) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "onelf: symlink target escapes package root",
            ));
        }
        let link_path = pkg_dir.join(&rel);

        if let Some(parent) = link_path.parent() {
            fs::create_dir_all(parent)?;
        }
        if link_path.symlink_metadata().is_ok() {
            fs::remove_file(&link_path)?;
        }
        std::os::unix::fs::symlink(target, &link_path)?;
    }

    Ok(())
}

fn touch_meta(meta_dir: &Path, package_id: &str) {
    let _ = fs::create_dir_all(meta_dir);
    let meta_path = meta_dir.join(package_id);
    let _ = fs::File::create(&meta_path);
}

pub(crate) fn remove_package(base: &Path, package_id: &str) {
    // Retire the completion marker before the tree so a concurrent reader
    // never sees the marker without its pkg_dir.
    let _ = fs::remove_file(layout::ready_marker(base, package_id));
    let _ = fs::remove_dir_all(layout::pkg_dir(base, package_id));
    let _ = fs::remove_file(layout::meta_path(base, package_id));
    let _ = fs::remove_file(layout::extract_lock_path(base, package_id));
    // The in-use lock file is intentionally kept: the flock protocol keys
    // mutual exclusion on its inode, so unlinking it would let a concurrent
    // run create a fresh inode and defeat the lock.
}

/// Remove content blobs that no package directory hardlinks any more, and
/// return the number of bytes reclaimed.
///
/// [`extract_to_cas`] populates a package by hardlinking each blob out of the
/// store, so the filesystem's own link count *is* the reference count: a blob
/// reachable only through the store itself has `nlink == 1`. That makes
/// collection correct without a side index, and it stays correct because the
/// store and the package directories are always on one filesystem (both are
/// children of the single resolved cache root, and a cross-device hardlink
/// would already have failed at extraction time).
pub(crate) fn collect_cas(base: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;

    let Ok(lock) = open_lock_inheritable(&layout::cas_lock_path(base)) else {
        return 0;
    };
    if rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_err() {
        return 0;
    }

    let Ok(shards) = fs::read_dir(base.join("cas")) else {
        return 0;
    };
    let mut reclaimed = 0u64;
    for shard in shards.flatten() {
        let Ok(blobs) = fs::read_dir(shard.path()) else {
            continue;
        };
        for blob in blobs.flatten() {
            let Ok(md) = blob.metadata() else { continue };
            if !md.is_file() || md.nlink() > 1 {
                continue;
            }
            let len = md.len();
            if fs::remove_file(blob.path()).is_ok() {
                reclaimed += len;
            }
        }
    }
    reclaimed
}

/// The view store of a cache root: where cache-mode execution views are
/// published, `views/view-<key>/`, beside the coordinating lock files the
/// view module puts there (`view-<key>.lease`, `view-<key>.create`) and
/// the staging trees a creator fills. The CLI sweep applies the same
/// rules to the same layout.
fn view_store(base: &Path) -> PathBuf {
    base.join("views")
}

/// Open a view store's coordinating lock file without truncating it. The
/// protocol keys every claim on the file's inode, so a collector that
/// had to create one leaves it behind for the next claimant.
fn open_lock_file(path: &Path) -> io::Result<fs::File> {
    fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

/// Reclaim the store's idle views and their scratch, and return how many
/// view identities were reclaimed.
///
/// A view is idle when neither of its two locks can be taken, both
/// nonblocking: the in-use lease proves a launch or its guardian is
/// running from the view, and the creation mutex proves a creator is
/// staging one. Either held means live, and everything of that identity
/// is skipped. With both taken, the view directory and its staging
/// trees go, whatever their state: a complete published view, a stale
/// one whose ready record is missing or damaged, and the scratch a dead
/// creator left are all idle derivable state that the next launch
/// rebuilds.
///
/// The coordinating lock inodes survive every sweep, held or not: the
/// flock protocol keys mutual exclusion on the inode, and unlinking one
/// would let a process that opened the old inode run beside a process
/// that creates a fresh one, splitting the view's lease.
///
/// No store lock is taken here, and none may be held by the caller:
/// canonical lock order puts the store's lock ahead of every view lock,
/// so a collector that held the store's lock while waiting for a view's
/// could cycle against a creator that keeps the store's exclusive lock
/// through its whole creation. Every view lock is nonblocking, so this
/// cannot wait on anyone at all.
fn collect_views(store: &Path) -> usize {
    let Ok(entries) = fs::read_dir(store) else {
        return 0;
    };
    // Only a view directory or a staging tree makes an identity worth
    // visiting; a bare pair of retained lock files is nothing to reclaim.
    let identities: std::collections::BTreeSet<String> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if let Some((base, _)) = name.split_once(crate::links::view::STAGING_SEP) {
                Some(base.to_string())
            } else if entry.file_type().is_ok_and(|t| t.is_dir()) {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    identities
        .into_iter()
        .filter(|name| reclaim_view(store, name))
        .count()
}

/// Reclaim one view identity when its locks prove it idle, and return
/// whether anything was removed. Holds the identity's in-use lease and
/// creation mutex exclusively across the removal, so a launch can start
/// using the view only after it is gone and its creator only after the
/// tree it was staging is history.
fn reclaim_view(store: &Path, name: &str) -> bool {
    use rustix::fs::FlockOperation;

    let view = store.join(name);
    let Some(lease_path) = crate::links::view::lease_lock_path(&view) else {
        return false;
    };
    let Some(create_path) = crate::links::view::creation_lock_path(&view) else {
        return false;
    };
    let lease = match open_lock_file(&lease_path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    if rustix::fs::flock(&lease, FlockOperation::NonBlockingLockExclusive).is_err() {
        return false;
    }
    let creation = match open_lock_file(&create_path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    if rustix::fs::flock(&creation, FlockOperation::NonBlockingLockExclusive).is_err() {
        return false;
    }

    let mut reclaimed = fs::remove_dir_all(&view).is_ok();
    let prefix = format!("{name}{}", crate::links::view::STAGING_SEP);
    if let Ok(entries) = fs::read_dir(store) {
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if !name.starts_with(&prefix) {
                continue;
            }
            let path = store.join(&name);
            if fs::remove_dir_all(&path).is_ok() || fs::remove_file(&path).is_ok() {
                reclaimed = true;
            }
        }
    }
    // Dropping the locks releases the identity; the inodes stay on disk.
    reclaimed
}

/// Sweep the cache for this package's idle state and everything older
/// than `max_age_secs`, reclaiming the base extraction, its blobs, and
/// the views it pinned that no live lease holds. A `current_pkg_id` of
/// an in-use package is never dropped by the sweep here.
pub fn auto_gc(base: &Path, max_age_secs: u64, current_pkg_id: &str) {
    // Views never age: a live lease proves liveness, and an idle view is
    // reclaimable however recently it was built. They go first, so the
    // store blobs they pinned become collectable in the same sweep, and
    // without touching the store lock, whose place in the canonical order
    // is ahead of every view lock.
    let views = collect_views(&view_store(base));

    let mut removed = 0u32;
    if let Ok(entries) = fs::read_dir(base.join("meta")) {
        let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
            return;
        };
        let now = now.as_secs();
        for entry in entries.flatten() {
            if removed >= 5 {
                break;
            }

            let name = entry.file_name();
            let id = name.to_string_lossy();
            // Never remove the current package.
            if id == current_pkg_id {
                continue;
            }

            let mtime = match entry.metadata().and_then(|m| m.modified()) {
                Ok(t) => t
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                Err(_) => continue,
            };

            if now.saturating_sub(mtime) > max_age_secs && try_remove_locked(base, &id) {
                removed += 1;
            }
        }
    }

    if removed > 0 || views > 0 {
        collect_cas(base);
    }
}

/// Acquire the exclusive lock for `id` and, while holding it, remove the
/// package. A running instance holds a shared lock, so the non-blocking
/// exclusive acquisition fails and the package is left untouched. The lock
/// is held across [`remove_package`] so a run cannot start mid-deletion.
/// Returns true if the package was removed.
fn try_remove_locked(base: &Path, id: &str) -> bool {
    let lock_path = layout::lock_path(base, id);
    let f = match fs::File::open(&lock_path) {
        Ok(f) => f,
        // A missing lock file means the package was never run under this
        // protocol, so it is safe to remove. Any other open failure
        // (permissions, fd exhaustion) means we cannot prove it is idle,
        // so skip it rather than risk deleting an in-use package.
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            remove_package(base, id);
            return true;
        }
        Err(_) => return false,
    };
    if rustix::fs::flock(&f, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_err() {
        return false; // held by a running process
    }
    remove_package(base, id);
    true
}

/// The cache root this uid may use, or `None` when no safe location
/// can be established from the environment.
pub fn base_dir() -> Option<PathBuf> {
    cache_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_hashes_to_detects_poisoning() {
        let dir = std::env::temp_dir().join(format!("onelf-cas-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("blob");

        let good = b"the real library bytes";
        fs::write(&path, good).unwrap();
        let good_hash = *blake3::hash(good).as_bytes();
        assert!(file_hashes_to(&path, &good_hash));

        // A poisoned slot (wrong bytes for the expected hash) is rejected.
        fs::write(&path, b"malicious replacement").unwrap();
        assert!(!file_hashes_to(&path, &good_hash));

        let _ = fs::remove_dir_all(&dir);
    }

    /// The store is reference-counted by the filesystem: a blob hardlinked
    /// into a package tree must survive, an unreferenced one is reclaimed,
    /// and nothing is touched while an extraction holds the store lock.
    #[test]
    fn collect_cas_respects_links_and_the_store_lock() {
        use rustix::fs::FlockOperation;
        let _locking = crate::test_locks::locking();

        let base = std::env::temp_dir().join(format!("onelf-cas-gc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let shard = base.join("cas/ab");
        let pkg = base.join("pkg/somepkg");
        fs::create_dir_all(&shard).unwrap();
        fs::create_dir_all(&pkg).unwrap();
        fs::create_dir_all(base.join("lock")).unwrap();

        // Referenced by a package tree: must survive on link count alone.
        let linked = shard.join("aaaa");
        fs::write(&linked, b"still in use").unwrap();
        fs::hard_link(&linked, pkg.join("f")).unwrap();

        // Unreferenced: collectable.
        let orphan = shard.join("bbbb");
        fs::write(&orphan, b"nobody wants me").unwrap();

        // While an extraction holds the store lock shared, link counts are
        // not yet trustworthy, so collection must decline entirely.
        let extracting = open_lock_inheritable(&layout::cas_lock_path(&base)).unwrap();
        rustix::fs::flock(&extracting, FlockOperation::LockShared).unwrap();
        assert_eq!(collect_cas(&base), 0, "must not collect mid-extraction");
        assert!(orphan.exists(), "an orphan is spared while extraction runs");
        drop(extracting);

        // With the store quiescent, link counts decide.
        let reclaimed = collect_cas(&base);
        assert!(linked.exists(), "a hardlinked blob must survive");
        assert!(!orphan.exists(), "an unreferenced blob is reclaimed");
        assert_eq!(reclaimed, b"nobody wants me".len() as u64);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn gc_skips_locked_and_removes_idle() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("onelf-gc-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        for sub in ["pkg", "meta", "lock"] {
            fs::create_dir_all(base.join(sub)).unwrap();
        }

        // Two packages, both over-age (mtime pinned to the epoch).
        let old = std::fs::FileTimes::new()
            .set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1));
        for id in ["locked", "idle"] {
            fs::create_dir_all(base.join("pkg").join(id)).unwrap();
            fs::write(base.join("pkg").join(id).join("f"), b"x").unwrap();
            fs::File::create(base.join("lock").join(id)).unwrap();
            let m = fs::File::create(base.join("meta").join(id)).unwrap();
            m.set_times(old).unwrap();
            let _ =
                fs::set_permissions(base.join("meta").join(id), PermissionsExt::from_mode(0o644));
        }

        // Hold the "locked" package's lock, as a running process would.
        let held = fs::File::open(base.join("lock").join("locked")).unwrap();
        rustix::fs::flock(&held, rustix::fs::FlockOperation::LockExclusive).unwrap();

        auto_gc(&base, 0, "none");

        assert!(
            base.join("pkg").join("locked").exists(),
            "running package must survive GC"
        );
        assert!(
            !base.join("pkg").join("idle").exists(),
            "idle over-age package must be removed"
        );

        let _ = fs::remove_dir_all(&base);
    }

    /// A store under a scratch cache root, and the published view `name`
    /// in it: complete enough that the view module's own readiness check
    /// accepts it.
    fn fixture_store(tag: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("onelf-viewgc-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let store = view_store(&base);
        fs::create_dir_all(&store).unwrap();
        (base, store)
    }

    /// Publish view `name` in `store`: a tree with content, a
    /// configuration record, and the exact ready record this layout
    /// writes.
    fn publish_view(store: &Path, name: &str) -> PathBuf {
        use crate::links::view;
        let dir = store.join(name);
        fs::create_dir_all(dir.join("lib")).unwrap();
        fs::write(dir.join("lib/app.so"), b"view payload").unwrap();
        fs::write(
            dir.join(view::CONFIGURATION_FILE),
            b"onelf-view 1\nuid 1000\n",
        )
        .unwrap();
        fs::write(
            dir.join(view::READY_FILE),
            format!("onelf-view-ready {}\n", view::LAYOUT),
        )
        .unwrap();
        assert!(view::ready(&dir), "the fixture view must be complete");
        dir
    }

    /// Hold view `name`'s in-use lease the way a launch does: create the
    /// inode and take it shared. The descriptor is the whole claim; the
    /// collector keys idleness on it, not on any recorded pid.
    fn hold_lease(store: &Path, name: &str) -> fs::File {
        use rustix::fs::FlockOperation;
        let view = store.join(name);
        let path = crate::links::view::lease_lock_path(&view).expect("a lease path");
        let file = open_lock_file(&path).expect("the lease inode");
        rustix::fs::flock(&file, FlockOperation::LockShared).expect("the lease");
        file
    }

    /// Whether `path` shows up within `secs` seconds.
    fn wait_for(path: &Path, secs: u64) -> bool {
        for _ in 0..secs * 20 {
            if path.exists() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        path.exists()
    }

    /// A view whose lease is taken is live state, whatever its directory
    /// holds: collection must skip it whole, and reclaim it only once the
    /// lease is released. flock arbitration is per open file description,
    /// so this second descriptor conflicts exactly as another process's
    /// lease would.
    #[test]
    fn a_leased_view_survives_collection_until_its_lease_is_released() {
        use rustix::fs::FlockOperation;
        let _locking = crate::test_locks::locking();
        let (base, store) = fixture_store("leased");
        let dir = publish_view(&store, "view-aa");

        let lease = hold_lease(&store, "view-aa");
        // A second descriptor cannot take what the launch holds, which is
        // the test a collector runs before touching anything.
        let probe_path =
            crate::links::view::lease_lock_path(&store.join("view-aa")).expect("a lease path");
        let probe = open_lock_file(&probe_path).expect("the probe descriptor");
        assert!(
            rustix::fs::flock(&probe, FlockOperation::NonBlockingLockExclusive).is_err(),
            "a held lease is not takeable"
        );
        drop(probe);

        assert_eq!(
            collect_views(&store),
            0,
            "a view with a live lease must be skipped"
        );
        assert!(
            crate::links::view::ready(&dir),
            "collection left the live view whole"
        );
        assert!(dir.join("lib/app.so").is_file());

        drop(lease);
        assert_eq!(
            collect_views(&store),
            1,
            "the view is idle once its lease is released"
        );
        assert!(!dir.exists(), "the idle view is reclaimed");

        let _ = fs::remove_dir_all(&base);
    }

    /// The lease of another process is invisible to this one except
    /// through the inode: a real holder in a process of its own must
    /// spare the view exactly as an in-process descriptor does.
    #[test]
    fn a_lease_held_by_another_process_spares_the_view() {
        let _forking = crate::test_locks::forking();
        let (base, store) = fixture_store("xproc");
        let dir = publish_view(&store, "view-bb");
        let up = base.join("up");
        let drop = base.join("drop");

        let mut child =
            std::process::Command::new(std::env::current_exe().expect("the test binary's path"))
                .arg("cache::tests::worker_holds_a_view_lease")
                .args(["--exact", "--ignored"])
                .env("ONELF_TEST_VIEW_STORE", &store)
                .env("ONELF_TEST_VIEW_NAME", "view-bb")
                .env("ONELF_TEST_VIEW_UP", &up)
                .env("ONELF_TEST_VIEW_DROP", &drop)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn the lease holder");

        assert!(wait_for(&up, 30), "the other process never took the lease");
        assert_eq!(
            collect_views(&store),
            0,
            "another process's lease must spare the view"
        );
        assert!(crate::links::view::ready(&dir));

        fs::write(&drop, b"").expect("end the launch");
        assert!(
            child.wait().expect("wait for the holder").success(),
            "the lease holder failed"
        );
        assert_eq!(collect_views(&store), 1, "the view is idle at last");
        assert!(!dir.exists());

        let _ = fs::remove_dir_all(&base);
    }

    /// Runs in its own process: holds the fixture view's lease shared
    /// until the driver ends the launch.
    #[test]
    #[ignore = "driven by a_lease_held_by_another_process_spares_the_view, in a process of its own"]
    fn worker_holds_a_view_lease() {
        let store = PathBuf::from(std::env::var("ONELF_TEST_VIEW_STORE").expect("the store"));
        let name = std::env::var("ONELF_TEST_VIEW_NAME").expect("the view name");
        let up = PathBuf::from(std::env::var("ONELF_TEST_VIEW_UP").expect("the up path"));
        let drop = PathBuf::from(std::env::var("ONELF_TEST_VIEW_DROP").expect("the drop path"));
        let _lease = hold_lease(&store, &name);
        fs::write(&up, b"").expect("announce the lease");
        for _ in 0..600 {
            if drop.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("the driver never ended the launch");
    }

    /// An idle view goes, its staging scratch goes with it, and the
    /// coordinating lock inodes stay for the next claimant: the flock
    /// protocol keys every claim on the inode, so a sweep that unlinked
    /// one would let two processes hold competing leases on one view.
    #[test]
    fn an_idle_view_is_reclaimed_but_its_lock_inodes_survive_the_sweep() {
        let _locking = crate::test_locks::locking();
        let (base, store) = fixture_store("locks");
        let dir = publish_view(&store, "view-cc");
        let view = store.join("view-cc");
        let lease_path = crate::links::view::lease_lock_path(&view).expect("a lease path");
        let create_path = crate::links::view::creation_lock_path(&view).expect("a creation path");
        drop(open_lock_file(&lease_path).expect("the lease inode"));
        drop(open_lock_file(&create_path).expect("the creation inode"));
        // The scratch a dead creator left.
        let staging = store.join(format!("view-cc{}42-0", crate::links::view::STAGING_SEP));
        fs::create_dir_all(staging.join("lib")).unwrap();
        fs::write(staging.join("lib/half.so"), b"half").unwrap();

        assert_eq!(collect_views(&store), 1, "the idle identity is reclaimed");
        assert!(!dir.exists(), "the idle view directory is reclaimed");
        assert!(!staging.exists(), "the dead creator's staging is reclaimed");
        assert!(
            lease_path.exists(),
            "the lease inode is retained across the sweep"
        );
        assert!(
            create_path.exists(),
            "the creation mutex inode is retained across the sweep"
        );

        // A later sweep finds nothing to reclaim and still keeps them.
        assert_eq!(collect_views(&store), 0);
        assert!(lease_path.exists() && create_path.exists());

        let _ = fs::remove_dir_all(&base);
    }

    /// A stale ready record marks scratch, not a publication: an idle
    /// view whose ready record is damaged or of another layout is not a
    /// live view under any identity, and collection reclaims it whole.
    #[test]
    fn a_stale_ready_record_marks_an_idle_view_for_reclamation() {
        use crate::links::view;
        let _locking = crate::test_locks::locking();
        let (base, store) = fixture_store("stale");
        let dir = store.join("view-dd");
        fs::create_dir_all(dir.join("lib")).unwrap();
        fs::write(dir.join("lib/app.so"), b"payload").unwrap();
        fs::write(
            dir.join(view::CONFIGURATION_FILE),
            b"onelf-view 1\nuid 1000\n",
        )
        .unwrap();
        fs::write(dir.join(view::READY_FILE), b"onelf-view-ready 0\n").unwrap();
        assert!(!view::ready(&dir), "the stale record is no publication");

        assert_eq!(collect_views(&store), 1);
        assert!(!dir.exists(), "the stale view is reclaimed");

        let _ = fs::remove_dir_all(&base);
    }

    /// The launch is gone, its guardian still holds the lease, and the
    /// snapshot the view was linked from must survive both sweeps while
    /// that descendant lives. Only when the last lease is released and
    /// the view is reclaimed does the content store reclaim the bytes.
    #[test]
    fn a_guardian_lease_keeps_a_view_and_the_snapshot_it_references() {
        let _locking = crate::test_locks::locking();
        let base = std::env::temp_dir().join(format!("onelf-viewgc-guard-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        for sub in ["cas/ab", "lock", "views"] {
            fs::create_dir_all(base.join(sub)).unwrap();
        }
        let blob = base.join("cas/ab").join("cc".repeat(32));
        fs::write(&blob, b"snapshot bytes").unwrap();
        let store = view_store(&base);
        let dir = publish_view(&store, "view-ee");
        fs::hard_link(&blob, dir.join("lib/snap.so")).expect("link the snapshot in");

        // The launcher has exited; the guardian inherited this descriptor
        // and holds the claim independently of anything recorded on disk.
        let lease = hold_lease(&store, "view-ee");

        assert_eq!(
            collect_views(&store),
            0,
            "a live descendant's view must be skipped"
        );
        assert!(dir.exists());
        assert_eq!(
            collect_cas(&base),
            0,
            "the snapshot a live view references must be retained"
        );
        assert!(blob.exists());

        drop(lease);
        assert_eq!(collect_views(&store), 1);
        assert!(!dir.exists());
        assert_eq!(
            collect_cas(&base),
            b"snapshot bytes".len() as u64,
            "the blob is collectable once its last reference is gone"
        );
        assert!(!blob.exists());

        let _ = fs::remove_dir_all(&base);
    }

    /// A creating launch keeps the store's exclusive lock through its
    /// whole creation, the canonical order allowing no other arrangement,
    /// and holds its lease and creation mutex while at it. Collection
    /// must run to completion beside it: the view pass may take no store
    /// lock at all (a store lock held while a view lock is awaited is the
    /// reversed cycle), and every view lock it takes is nonblocking. The
    /// watchdog turns a violation into a failed test instead of a hung
    /// suite.
    #[test]
    fn a_collector_runs_beside_a_creating_launch_without_deadlock() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;

        let _locking = crate::test_locks::locking();
        let base = std::env::temp_dir().join(format!("onelf-viewgc-dead-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        for sub in ["cas/ab", "lock", "meta", "views"] {
            fs::create_dir_all(base.join(sub)).unwrap();
        }
        let store = view_store(&base);
        let dir = publish_view(&store, "view-ff");
        let orphan = base.join("cas/ab").join("dd".repeat(32));
        fs::write(&orphan, b"orphan bytes").unwrap();

        let cas_path = layout::cas_lock_path(&base);
        let view = store.join("view-ff");
        let lease_path = crate::links::view::lease_lock_path(&view).expect("a lease path");
        let create_path = crate::links::view::creation_lock_path(&view).expect("a create path");
        let (held_tx, held_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let creator = std::thread::spawn(move || {
            use rustix::fs::FlockOperation;
            let store_lock = open_lock_inheritable(&cas_path).expect("the store lock");
            rustix::fs::flock(&store_lock, FlockOperation::LockExclusive).expect("the store");
            let lease = open_lock_file(&lease_path).expect("the lease inode");
            rustix::fs::flock(&lease, FlockOperation::LockShared).expect("the lease");
            let creation = open_lock_file(&create_path).expect("the creation inode");
            rustix::fs::flock(&creation, FlockOperation::LockExclusive).expect("the mutex");
            held_tx.send(()).expect("report the locks");
            let _ = release_rx.recv();
        });
        held_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the creating launch took its locks");

        let disarmed = Arc::new(AtomicBool::new(false));
        let disarm = disarmed.clone();
        let watchdog = std::thread::spawn(move || {
            for _ in 0..150 {
                if disarm.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            eprintln!("collection deadlocked against a concurrent creation");
            std::process::exit(101);
        });

        // Both passes answer immediately beside the creator: the view is
        // skipped whole, the store pass declines, the orphan survives.
        assert_eq!(
            collect_views(&store),
            0,
            "a view under a live creation must be skipped"
        );
        assert!(
            crate::links::view::ready(&dir),
            "the skipped view is untouched"
        );
        assert_eq!(
            collect_cas(&base),
            0,
            "the content pass declines while creation holds the store"
        );
        assert!(orphan.exists(), "the orphan is spared mid-creation");

        release_tx.send(()).expect("release the creator");
        creator.join().expect("the creator finished");
        disarmed.store(true, Ordering::SeqCst);
        watchdog.join().expect("the watchdog finished");

        // With the creation over, one launch's own sweep reclaims the
        // view and the bytes it pinned.
        auto_gc(&base, 0, "none");
        assert!(!dir.exists(), "the finished view is reclaimed");
        assert!(
            !orphan.exists(),
            "the bytes the view pinned are collectable once it is gone"
        );

        let _ = fs::remove_dir_all(&base);
    }
}

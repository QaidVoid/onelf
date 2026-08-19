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
/// combined with the pid it is unique across concurrent extractions.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

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
/// the packer CLI applies exactly the same ones.
fn cache_dir() -> Option<PathBuf> {
    onelf_format::cache_layout::resolve_root(
        rustix::process::getuid().as_raw(),
        crate::paths::private_dir(),
    )
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Extract all package contents directly into `target_dir` without CAS.
/// Used for ephemeral tmpfs-backed extraction inside a private namespace,
/// where dedup has no value and the whole tree is thrown away on exit.
pub fn extract_direct(pkg: &mut PackageData, target_dir: &Path) -> io::Result<()> {
    let manifest = &pkg.manifest;

    // Dirs first
    for (i, entry) in manifest.entries.iter().enumerate() {
        if entry.kind == EntryKind::Dir {
            let rel = manifest.validated_entry_path(i)?;
            if rel.as_os_str().is_empty() {
                continue;
            }
            fs::create_dir_all(target_dir.join(&rel))?;
        }
    }

    // Files (before symlinks, so no file is written through a symlink).
    for (i, entry) in manifest.entries.iter().enumerate() {
        if entry.kind != EntryKind::File {
            continue;
        }
        let rel = manifest.validated_entry_path(i)?;
        let out_path = target_dir.join(&rel);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let data =
            loader::read_verified_entry(&mut pkg.file, &pkg.footer, entry, pkg.dict.as_deref())?;

        let mut f = fs::File::create(&out_path)?;
        f.write_all(&data)?;
        f.set_permissions(fs::Permissions::from_mode(entry.mode & 0o777))?;
    }

    // Symlinks last; refuse any target that escapes the root.
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
        let tmp_dir = pkg_parent.join(format!(".{package_id}.tmp"));
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
    for (i, entry) in manifest.entries.iter().enumerate() {
        if entry.kind != EntryKind::File {
            continue;
        }

        // Validate the link path before any I/O so a hostile name is
        // rejected before it can create directories.
        let rel = manifest.validated_entry_path(i)?;

        let hash_hex = hex(&entry.content_hash);
        let shard = &hash_hex[..2];
        let cas_shard_dir = cas_dir.join(shard);
        let cas_path = cas_shard_dir.join(&hash_hex);

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
            let data = loader::read_verified_entry(
                &mut pkg.file,
                &pkg.footer,
                entry,
                pkg.dict.as_deref(),
            )?;

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
                f.set_permissions(fs::Permissions::from_mode(entry.mode & 0o777))?;
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

pub fn remove_package(base: &Path, package_id: &str) {
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
pub fn collect_cas(base: &Path) -> u64 {
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

pub fn auto_gc(base: &Path, max_age_secs: u64, current_pkg_id: &str) {
    let meta_dir = base.join("meta");
    let entries = match fs::read_dir(&meta_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let now = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs(),
        Err(_) => return,
    };

    let mut removed = 0u32;
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

    if removed > 0 {
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
}

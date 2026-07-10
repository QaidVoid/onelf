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

/// Resolve the persistent cache root. Prefers `$XDG_CACHE_HOME`, then
/// `~/.cache` (both already user-private). Only when neither is set does it
/// fall back to the `0700` per-uid private dir; if even that is unavailable
/// it returns `None` so the caller refuses rather than using shared `/tmp`.
fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .or_else(crate::paths::private_dir)?;
    Some(base.join("onelf"))
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

pub fn ensure_extracted(pkg: &mut PackageData) -> io::Result<PathBuf> {
    let base = cache_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "onelf: no safe cache directory (set HOME or XDG_RUNTIME_DIR)",
        )
    })?;
    let package_id = hex(&pkg.manifest.header.package_id);
    let pkg_dir = base.join("pkg").join(&package_id);
    let cas_dir = base.join("cas");
    let lock_dir = base.join("lock");
    let meta_dir = base.join("meta");

    // Fast path: already extracted
    if pkg_dir.exists() {
        touch_meta(&meta_dir, &package_id);
        return Ok(pkg_dir);
    }

    // Take lock
    fs::create_dir_all(&lock_dir)?;
    let lock_path = lock_dir.join(&package_id);
    let lock_file = fs::File::create(&lock_path)?;
    rustix::fs::flock(&lock_file, rustix::fs::FlockOperation::LockExclusive)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("flock: {e}")))?;

    // Double-check after acquiring lock
    if pkg_dir.exists() {
        touch_meta(&meta_dir, &package_id);
        return Ok(pkg_dir);
    }

    fs::create_dir_all(&cas_dir)?;
    let pkg_parent = base.join("pkg");
    fs::create_dir_all(&pkg_parent)?;

    // Extract into a per-package temp dir, then atomically rename it to
    // pkg_dir. The lock-free fast path gates on pkg_dir existing, so a
    // concurrent run never observes a half-populated tree: the completed
    // directory appears in a single step. A stale temp from a crashed
    // extraction is removed first, and any failure removes the temp so
    // the next run re-extracts rather than reusing a partial tree.
    let tmp_dir = pkg_parent.join(format!(".{package_id}.tmp"));
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir)?;

    if let Err(e) = extract_to_cas(pkg, &cas_dir, &tmp_dir) {
        let _ = fs::remove_dir_all(&tmp_dir);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp_dir, &pkg_dir) {
        let _ = fs::remove_dir_all(&tmp_dir);
        return Err(e);
    }

    // Record metadata
    touch_meta(&meta_dir, &package_id);

    // Lock released when lock_file goes out of scope
    Ok(pkg_dir)
}

fn walk_files(dir: &Path, f: &mut dyn FnMut(&Path)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            walk_files(&path, f);
        } else if meta.is_file() {
            f(&path);
        }
    }
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
    let _ = fs::remove_dir_all(base.join("pkg").join(package_id));
    let _ = fs::remove_file(base.join("meta").join(package_id));
    let _ = fs::remove_file(base.join("lock").join(package_id));
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
        // Never remove the current package, nor any package a concurrent
        // process is actively running (its lock is held).
        if id == current_pkg_id || package_is_locked(base, &id) {
            continue;
        }

        let mtime = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            Err(_) => continue,
        };

        if now.saturating_sub(mtime) > max_age_secs {
            remove_package(base, &id);
            removed += 1;
        }
    }
}

/// True if another process holds the exclusive lock for `id` (i.e. the
/// package is actively running). A non-blocking exclusive `flock` that
/// would block means the package is in use and must not be GC'd.
fn package_is_locked(base: &Path, id: &str) -> bool {
    let lock_path = base.join("lock").join(id);
    let Ok(f) = fs::File::open(&lock_path) else {
        return false; // no lock file -> not currently running
    };
    match rustix::fs::flock(&f, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => false, // acquired -> nobody else holds it
        Err(_) => true,  // would block -> held by a running process
    }
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
            let _ = fs::set_permissions(base.join("meta").join(id), PermissionsExt::from_mode(0o644));
        }

        // Hold the "locked" package's lock, as a running process would.
        let held = fs::File::open(base.join("lock").join("locked")).unwrap();
        rustix::fs::flock(&held, rustix::fs::FlockOperation::LockExclusive).unwrap();

        auto_gc(&base, 0, "none");

        assert!(base.join("pkg").join("locked").exists(), "running package must survive GC");
        assert!(!base.join("pkg").join("idle").exists(), "idle over-age package must be removed");

        let _ = fs::remove_dir_all(&base);
    }
}

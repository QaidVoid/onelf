//! Cache management for the onelf runtime extraction cache.
//!
//! Lists cached packages, clears the cache, and garbage-collects stale
//! entries. Every operation speaks the same protocol the runtime does: the
//! cache root is resolved by the shared rules, and a package is only removed
//! once its exclusive lock proves no instance is running it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use onelf_format::cache_layout as layout;

/// Resolve the cache root, or fail with a message. The packer offers no
/// fallback of its own: unlike the runtime it has no private per-uid scratch
/// directory to fall back to, and guessing a shared location is exactly what
/// this refuses to do.
fn cache_root() -> io::Result<PathBuf> {
    layout::resolve_root(rustix::process::getuid().as_raw(), None).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no safe cache directory (set HOME or XDG_CACHE_HOME to an absolute path)",
        )
    })
}

/// Remove `package_id` only if no running instance holds its lock, mirroring
/// the runtime's collector. Returns true when the package was removed.
///
/// The lock is held across the removal so an instance cannot start midway
/// through it. The in-use lock file itself is deliberately left behind: the
/// protocol keys mutual exclusion on its inode, so unlinking it would let a
/// concurrent run create a fresh inode and defeat the lock.
///
/// A missing lock file means the package was never run under this protocol,
/// so there is nothing to conflict with. Any other open failure leaves it
/// alone, since idleness cannot be proven.
fn try_remove_locked(root: &Path, package_id: &str) -> bool {
    let lock_path = layout::lock_path(root, package_id);
    let held = match fs::File::open(&lock_path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            remove_package(root, package_id);
            return true;
        }
        Err(_) => return false,
    };
    if rustix::fs::flock(&held, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_err() {
        return false; // a running instance holds it
    }
    remove_package(root, package_id);
    true
}

/// Remove a package's tree and its per-package protocol files.
fn remove_package(root: &Path, package_id: &str) {
    // Marker first: a concurrent reader must never see it without its tree.
    let _ = fs::remove_file(layout::ready_marker(root, package_id));
    let _ = fs::remove_dir_all(layout::pkg_dir(root, package_id));
    let _ = fs::remove_file(layout::meta_path(root, package_id));
    let _ = fs::remove_file(layout::extract_lock_path(root, package_id));
}

/// Remove content blobs no package directory hardlinks any more, returning
/// the bytes reclaimed. A blob reachable only through the store itself has a
/// link count of 1, so the filesystem already keeps the reference count.
///
/// Returns 0 without touching anything when an extraction is in flight: the
/// store lock is what makes link counts trustworthy, and a runtime that is
/// mid-extraction holds it shared.
fn collect_cas(root: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;

    let Ok(lock) = fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(layout::cas_lock_path(root))
    else {
        return 0;
    };
    if rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_err() {
        return 0;
    }

    let Ok(shards) = fs::read_dir(root.join("cas")) else {
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

/// Total bytes held by the content store.
fn cas_size(root: &Path) -> u64 {
    let Ok(shards) = fs::read_dir(root.join("cas")) else {
        return 0;
    };
    shards
        .flatten()
        .filter_map(|shard| fs::read_dir(shard.path()).ok())
        .flat_map(|blobs| blobs.flatten())
        .filter_map(|b| b.metadata().ok())
        .map(|m| m.len())
        .sum()
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / 1_048_576.0
}

/// Bytes held by every file under `dir`.
fn dir_size(dir: &Path) -> u64 {
    jwalk::WalkDir::new(dir)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// A pinned GL build the store holds: its label, when a package last used
/// it, and its size.
struct StoredBuild {
    label: String,
    dir: PathBuf,
    used: Option<SystemTime>,
    bytes: u64,
}

/// The builds the store holds, by label. A label directory without the
/// build file is a fetch that never completed and is listed as unused.
fn stored_builds(root: &Path) -> Vec<StoredBuild> {
    let Ok(entries) = fs::read_dir(layout::platform_dir(root)) else {
        return Vec::new();
    };
    let mut builds: Vec<StoredBuild> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| {
            let dir = e.path();
            StoredBuild {
                label: e.file_name().to_string_lossy().into_owned(),
                used: fs::metadata(dir.join("gl.onelf"))
                    .and_then(|m| m.modified())
                    .ok(),
                bytes: dir_size(&dir),
                dir,
            }
        })
        .collect();
    builds.sort_by(|a, b| a.label.cmp(&b.label));
    builds
}

pub fn cache_list() -> io::Result<()> {
    let root = cache_root()?;
    let now = SystemTime::now();
    let ago = |t: Option<SystemTime>| {
        t.and_then(|t| now.duration_since(t).ok())
            .map(|age| format!("{}s ago", age.as_secs()))
            .unwrap_or_else(|| "unknown".into())
    };

    let builds = stored_builds(&root);
    if !builds.is_empty() {
        println!("GL builds:");
        for build in &builds {
            println!(
                "  {} ({:.1} MB, last used: {})",
                build.label,
                mib(build.bytes),
                ago(build.used)
            );
        }
        println!();
    }

    let pkg_dir = root.join("pkg");
    if !pkg_dir.exists() {
        println!("No cached packages.");
        return Ok(());
    }

    let mut count = 0u64;

    for entry in fs::read_dir(&pkg_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let id = name.to_string_lossy();

        let last_used = ago(fs::metadata(layout::meta_path(&root, &id))
            .and_then(|m| m.modified())
            .ok());

        println!("  {id} (last used: {last_used})");
        count += 1;
    }

    println!();
    println!(
        "{count} packages, CAS total: {:.1} MB (run `onelf cache gc` to reclaim)",
        mib(cas_size(&root))
    );
    Ok(())
}

/// Remove the whole cache.
///
/// The recursive delete is safe because `cache_root` only ever yields a
/// `0700` directory owned by this user, never a shared or foreign path.
pub fn cache_clear() -> io::Result<()> {
    let root = cache_root()?;
    if root.exists() {
        fs::remove_dir_all(&root)?;
        println!("Cache cleared.");
    } else {
        println!("No cache to clear.");
    }
    Ok(())
}

pub fn cache_gc(max_age_days: u64) -> io::Result<()> {
    let root = cache_root()?;
    let now = SystemTime::now();
    let max_age = std::time::Duration::from_secs(max_age_days * 86400);

    // A build's file is touched on every use, so its age is its idleness.
    // Its extraction is a package like any other and goes through the
    // package pass below when idle.
    let mut builds_removed = 0u64;
    let mut build_bytes = 0u64;
    for build in stored_builds(&root) {
        let idle = match build.used.map(|t| now.duration_since(t)) {
            None => true,
            Some(Ok(age)) => age > max_age,
            Some(Err(_)) => false,
        };
        if idle && fs::remove_dir_all(&build.dir).is_ok() {
            builds_removed += 1;
            build_bytes += build.bytes;
        }
    }
    if builds_removed > 0 {
        println!(
            "Removed {builds_removed} GL build(s) unused for {max_age_days} days, \
             reclaimed {:.1} MB.",
            mib(build_bytes)
        );
    }

    let meta_dir = root.join("meta");
    if !meta_dir.exists() {
        println!("No cached packages.");
        return Ok(());
    }

    let mut removed = 0u64;
    let mut skipped = 0u64;

    for entry in fs::read_dir(&meta_dir)? {
        let entry = entry?;
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        let Ok(age) = now.duration_since(modified) else {
            continue; // future mtime: treat as freshly used
        };
        if age <= max_age {
            continue;
        }
        let name = entry.file_name();
        let id = name.to_string_lossy();
        if try_remove_locked(&root, &id) {
            removed += 1;
        } else {
            skipped += 1;
        }
    }

    let reclaimed = if removed > 0 { collect_cas(&root) } else { 0 };

    println!(
        "Removed {removed} stale packages (older than {max_age_days} days), \
         reclaimed {:.1} MB.",
        mib(reclaimed)
    );
    if skipped > 0 {
        println!("Skipped {skipped} package(s) still in use by a running instance.");
    }
    Ok(())
}

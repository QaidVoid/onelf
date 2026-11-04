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
    let _ = fs::remove_dir_all(layout::extract_tmp_dir(root, package_id));
    let _ = fs::remove_file(layout::meta_path(root, package_id));
    let _ = fs::remove_file(layout::extract_lock_path(root, package_id));
}

/// Package IDs the cache holds anything for: those with a last-used record,
/// and those with a tree, marker or extraction scratch in `pkg/`, since an
/// interrupted extraction leaves its scratch behind without a record.
fn cached_ids(root: &Path) -> std::collections::BTreeSet<String> {
    let mut ids = std::collections::BTreeSet::new();
    for dir in ["meta", "pkg"] {
        let Ok(entries) = fs::read_dir(root.join(dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let id = name
                .strip_prefix('.')
                .and_then(|n| n.strip_suffix(".tmp").or_else(|| n.strip_suffix(".ready")))
                .unwrap_or(&name);
            if !id.is_empty() && !id.starts_with('.') {
                ids.insert(id.to_string());
            }
        }
    }
    ids
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

/// Reclaim the idle views under `root` unused for more than
/// `max_idle_secs`, with the runtime's own collector, so both sides speak
/// one lock protocol over one layout.
fn collect_views(root: &Path, max_idle_secs: u64) -> usize {
    onelf_rt::cache::collect_views(&onelf_rt::cache::view_store(root), max_idle_secs)
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / 1_048_576.0
}

/// A pinned GL build the store holds: its label, the hash it is named by,
/// its file, when a package last used it, and its size. The store keys a
/// build by its hash, so one label may hold several as its pin moves on.
struct StoredBuild {
    label: String,
    hash: String,
    file: PathBuf,
    used: Option<SystemTime>,
    bytes: u64,
}

/// The builds the store holds: every `<hash>.onelf` under every label
/// directory. A partial download (a dotfile) is skipped.
fn stored_builds(root: &Path) -> Vec<StoredBuild> {
    let Ok(labels) = fs::read_dir(layout::platform_dir(root)) else {
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
            if hash.starts_with('.') {
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
                "  {} {} ({:.1} MB, last used: {})",
                build.label,
                build.hash.chars().take(16).collect::<String>(),
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

/// Remove everything in the cache that no running instance is using.
///
/// A package still running keeps its tree, since it opens plugins, locale
/// data, and drivers from there lazily for as long as it runs. Every GL
/// build file goes: a running instance uses the build's extraction, which
/// is a package like any other, not the file. Idle execution views and
/// their scratch go the same way, through the runtime's collector. Directories left empty are removed; the root itself
/// stays only while something in it is in use.
pub fn cache_clear() -> io::Result<()> {
    let root = cache_root()?;
    println!("{}", clear_cache(&root)?);
    Ok(())
}

/// [`cache_clear`] against one root, returning what it did. Separate from
/// the command entry so the reclamation rules can be exercised against a
/// scratch store without touching the process's own cache.
fn clear_cache(root: &Path) -> io::Result<String> {
    if !root.exists() {
        return Ok("No cache to clear.".into());
    }

    let mut removed = 0u64;
    let mut skipped = 0u64;
    for id in cached_ids(root) {
        if try_remove_locked(root, &id) {
            removed += 1;
        } else {
            skipped += 1;
        }
    }
    let mut builds = 0u64;
    for build in stored_builds(root) {
        if fs::remove_file(&build.file).is_ok() {
            builds += 1;
            if let Some(parent) = build.file.parent() {
                let _ = fs::remove_dir(parent);
            }
        }
    }
    // Views first, so the store bytes they pinned are collectable in the
    // same sweep, and without holding any lock the content pass needs.
    let views = collect_views(root, 0);
    let reclaimed = collect_cas(root);
    remove_empty_dirs(root);

    let mut report = Vec::new();
    if views > 0 {
        report.push(format!("Removed {views} idle view(s)."));
    }
    if skipped == 0 {
        report.push("Cache cleared.".into());
    } else {
        report.push(format!(
            "Removed {removed} package(s) and {builds} GL build(s), reclaimed {:.1} MB.",
            mib(reclaimed)
        ));
        report.push(format!(
            "Skipped {skipped} package(s) still in use by a running instance."
        ));
    }
    Ok(report.join("\n"))
}

/// Remove `dir` and every directory under it that holds nothing. Lock files
/// of running packages keep their directories, and the root with them.
fn remove_empty_dirs(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            remove_empty_dirs(&entry.path());
        }
    }
    let _ = fs::remove_dir(dir);
}

pub fn cache_gc(max_age_days: u64) -> io::Result<()> {
    let root = cache_root()?;
    println!("{}", gc_cache(&root, max_age_days)?);
    Ok(())
}

/// [`cache_gc`] against one root, returning what it did. Separate from
/// the command entry so the reclamation rules can be exercised against a
/// scratch store without touching the process's own cache.
fn gc_cache(root: &Path, max_age_days: u64) -> io::Result<String> {
    let mut report = Vec::new();
    let now = SystemTime::now();
    let max_age = std::time::Duration::from_secs(max_age_days.saturating_mul(86400));

    // A build's file is touched on every use, so its age is its idleness.
    // Its extraction is a package like any other and goes through the
    // package pass below when idle.
    let mut builds_removed = 0u64;
    let mut build_bytes = 0u64;
    for build in stored_builds(root) {
        let idle = match build.used.map(|t| now.duration_since(t)) {
            None => true,
            Some(Ok(age)) => age > max_age,
            Some(Err(_)) => false,
        };
        if idle && fs::remove_file(&build.file).is_ok() {
            builds_removed += 1;
            build_bytes += build.bytes;
            // Drop the label directory once its last build is gone.
            if let Some(parent) = build.file.parent() {
                let _ = fs::remove_dir(parent);
            }
        }
    }
    if builds_removed > 0 {
        report.push(format!(
            "Removed {builds_removed} GL build(s) unused for {max_age_days} days, \
             reclaimed {:.1} MB.",
            mib(build_bytes)
        ));
    }

    // An idle view ages from the last use its lease records, under the
    // same lock rules the runtime collector applies; a live lease keeps
    // it whatever its age.
    let views = collect_views(root, max_age_days.saturating_mul(86400));
    if views > 0 {
        report.push(format!("Removed {views} idle view(s)."));
    }

    let meta_dir = root.join("meta");
    if !meta_dir.exists() {
        report.push("No cached packages.".into());
        return Ok(report.join("\n"));
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
        if try_remove_locked(root, &id) {
            removed += 1;
        } else {
            skipped += 1;
        }
    }

    let reclaimed = if removed > 0 || views > 0 {
        collect_cas(root)
    } else {
        0
    };

    report.push(format!(
        "Removed {removed} stale packages (older than {max_age_days} days), \
         reclaimed {:.1} MB.",
        mib(reclaimed)
    ));
    if skipped > 0 {
        report.push(format!(
            "Skipped {skipped} package(s) still in use by a running instance."
        ));
    }
    Ok(report.join("\n"))
}
#[cfg(test)]
mod tests {
    use super::*;

    use rustix::fs::FlockOperation;

    /// The view store under `root`.
    fn store(root: &Path) -> PathBuf {
        onelf_rt::cache::view_store(root)
    }

    /// A scratch cache root, with the directories a store that has run
    /// anything would have.
    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("onelf-cliview-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for sub in ["cas/ab", "lock", "meta"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        fs::create_dir_all(store(&root)).unwrap();
        root
    }

    /// Publish view `name` in `root`'s view store, complete enough that
    /// the runtime's readiness check accepts it.
    fn publish_view(root: &Path, name: &str) -> PathBuf {
        let view = store(root).join(name);
        fs::create_dir_all(view.join("lib")).unwrap();
        fs::write(view.join("lib/app.so"), b"view payload").unwrap();
        fs::write(view.join("config"), b"onelf-view 1\nuid 1000\n").unwrap();
        fs::write(view.join("ready"), "onelf-view-ready 1\n").unwrap();
        assert!(onelf_rt::links::view::ready(&view));
        view
    }

    /// The lease inode of view `name`, opened without taking it.
    fn lease_file(root: &Path, name: &str) -> fs::File {
        let path = onelf_rt::links::view::lease_lock_path(&store(root).join(name)).unwrap();
        fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap()
    }

    /// Hold view `name`'s in-use lease the way a launch or its guardian
    /// does. The descriptor is the whole claim.
    fn hold_lease(root: &Path, name: &str) -> fs::File {
        let file = lease_file(root, name);
        rustix::fs::flock(&file, FlockOperation::LockShared).expect("the lease");
        file
    }

    /// A view whose lease a live launch holds is skipped whole by both
    /// sweep commands, the snapshot it references survives the content
    /// pass, and once the lease is released the idle view goes while its
    /// coordinating lock inodes and the store's own lock file stay.
    #[test]
    fn the_sweep_spares_a_leased_view_and_then_reclaims_it_idle() {
        let root = temp_root("leased");
        let view = publish_view(&root, "view-aa");
        let blob = root.join("cas/ab").join("cc".repeat(32));
        fs::write(&blob, b"snapshot bytes").unwrap();
        fs::hard_link(&blob, view.join("lib/snap.so")).unwrap();
        let lease = hold_lease(&root, "view-aa");

        let report = clear_cache(&root).unwrap();
        assert!(report.contains("Cache cleared."), "{report}");
        assert!(
            view.join("ready").is_file(),
            "a leased view must survive clear: {report}"
        );
        assert_eq!(collect_cas(&root), 0, "the referenced snapshot survives");
        assert!(blob.exists());

        let report = gc_cache(&root, 0).unwrap();
        assert!(
            !report.contains("idle view"),
            "a leased view is not idle, whatever its age: {report}"
        );
        assert!(view.join("ready").is_file());

        drop(lease);
        let report = clear_cache(&root).unwrap();
        assert!(
            report.contains("Removed 1 idle view(s)."),
            "the idle view is reclaimed: {report}"
        );
        assert!(!view.exists());
        assert!(
            !blob.exists(),
            "the bytes the view pinned went with the sweep that reclaimed it"
        );

        let views = store(&root);
        assert!(
            views.join("view-aa.lease").exists() && views.join("view-aa.create").exists(),
            "coordinating lock inodes survive the sweep"
        );
        assert!(
            root.join("lock/cas").exists(),
            "the store's lock inode survives the sweep"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// `gc` keeps an idle view its lease records as used within
    /// `--max-age` and reclaims it once that use is older, while a stale
    /// view and a dead creator's scratch go whatever the age, and a held
    /// lease keeps its view.
    #[test]
    fn the_sweep_ages_idle_views_and_reclaims_stale_scratch() {
        let root = temp_root("idle");
        let idle = publish_view(&root, "view-bb");
        let used = lease_file(&root, "view-bb");
        let stale = store(&root).join("view-cc");
        fs::create_dir_all(&stale).unwrap();
        fs::write(stale.join("config"), b"onelf-view 1\nuid 1000\n").unwrap();
        fs::write(stale.join("ready"), b"onelf-view-ready 0\n").unwrap();
        let staging = store(&root).join("view-bb.staging-42-0");
        fs::create_dir_all(staging.join("lib")).unwrap();
        fs::write(staging.join("lib/half.so"), b"half").unwrap();
        let live = publish_view(&root, "view-dd");
        let lease = hold_lease(&root, "view-dd");

        let report = gc_cache(&root, 30).unwrap();
        assert!(
            report.contains("Removed 1 idle view(s)."),
            "only the stale view goes while the idle one is recent: {report}"
        );
        assert!(idle.join("ready").is_file(), "a recently used view stays");
        assert!(!stale.exists());
        assert!(!staging.exists());
        assert!(live.join("ready").is_file());

        let old = SystemTime::now() - std::time::Duration::from_secs(31 * 86400);
        used.set_modified(old).unwrap();
        drop(used);
        drop(lease);
        lease_file(&root, "view-dd").set_modified(old).unwrap();
        let report = gc_cache(&root, 30).unwrap();
        assert!(
            report.contains("Removed 2 idle view(s)."),
            "views unused past the age go: {report}"
        );
        assert!(!idle.exists());
        assert!(!live.exists());

        let _ = fs::remove_dir_all(&root);
    }
}

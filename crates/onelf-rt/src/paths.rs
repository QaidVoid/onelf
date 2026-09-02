//! Private per-user scratch directory resolution.
//!
//! Runtime scratch state (PT_INTERP symlinks, FUSE/ephemeral mountpoints,
//! and the cache fallback) must never live at a shared, world-writable
//! path like `/tmp/onelf`, where another local user could pre-create or
//! tamper with entries. [`private_dir`] returns a `0700`, current-uid-owned
//! base instead.

use std::path::{Path, PathBuf};

use std::os::unix::fs::DirBuilderExt;

/// Current real uid.
fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// True if `path` is a real directory (not a symlink) owned by the current
/// uid with no group/other permission bits.
fn is_safe_owned_dir(path: &Path) -> bool {
    onelf_format::cache_layout::is_safe_owned_dir(path, uid())
}

/// Return a `0700`, current-uid-owned scratch base directory, or `None`
/// when no safe location can be established.
///
/// Prefers `$XDG_RUNTIME_DIR` (systemd already makes it `0700` per-uid);
/// otherwise creates `/tmp/onelf-<uid>` with mode `0700` and confirms via
/// `symlink_metadata` that it is a real directory owned by us. A
/// pre-existing path that is a symlink or owned by another user is refused
/// (never chmod'd through), so the caller falls back to failing closed.
pub fn private_dir() -> Option<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(x);
        if is_safe_owned_dir(&p) {
            return Some(p);
        }
    }

    let p = PathBuf::from(format!("/tmp/onelf-{}", uid()));
    // Create with mode 0700 atomically via mkdir(2) so there is never a
    // window where the directory is group/other-accessible. umask can only
    // clear bits, so the result is never looser than 0700.
    match std::fs::DirBuilder::new().mode(0o700).create(&p) {
        Ok(()) => {}
        // Pre-existing: do NOT chmod (could be a symlink); verify below.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return None,
    }

    is_safe_owned_dir(&p).then_some(p)
}

/// Open a lock file leaving the descriptor inheritable (no `CLOEXEC`), so a
/// lock taken before an `exec` still guards the resource afterwards. `std`'s
/// `File::open` sets `CLOEXEC`, which would drop the claim at exec time.
pub(crate) fn open_lock_inheritable(path: &Path) -> std::io::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags};
    let fd = rustix::fs::open(path, OFlags::CREATE | OFlags::RDWR, Mode::RUSR | Mode::WUSR)?;
    Ok(std::fs::File::from(fd))
}

/// The lock file guarding the mountpoint directory `dir_name`. Kept beside
/// the directory it guards, so both live and die with the private dir.
fn mountpoint_lock_path(base: &Path, dir_name: &str) -> PathBuf {
    base.join(format!(".{dir_name}.lock"))
}

/// A mountpoint directory together with the shared lock claiming it.
///
/// The lock must outlive every use of the directory. While it is held, a
/// concurrently starting instance cannot take the exclusive lock its sweep
/// requires, so it cannot remove the directory out from under us. The
/// descriptor is inheritable so the claim survives the `exec` in tmpfs mode.
pub struct Mountpoint {
    pub path: PathBuf,
    _lock: std::fs::File,
}

impl Mountpoint {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Create a per-package mountpoint / extraction directory under the private
/// runtime dir, named `onelf-<name prefix>-<id prefix>`. Shared by the FUSE
/// and ephemeral tmpfs modes; returns `None` if no private dir is available
/// or the directory cannot be created.
///
/// The shared lock is taken *before* the directory is created, which closes
/// the window where a freshly created but not yet mounted directory looks
/// exactly like an abandoned one to a concurrent sweep.
pub fn create_mountpoint(package_name: &str, package_id: &[u8; 32]) -> Option<Mountpoint> {
    use rustix::fs::FlockOperation;

    let name_prefix: String = package_name.chars().take(6).collect();
    let hash_suffix = crate::cache::hex(&package_id[0..4]);
    let dir_name = format!("onelf-{name_prefix}-{hash_suffix}");

    let base = private_dir()?;
    let lock_path = mountpoint_lock_path(&base, &dir_name);
    let lock = match open_lock_inheritable(&lock_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("onelf-rt: cannot open {}: {e}", lock_path.display());
            return None;
        }
    };
    if let Err(e) = rustix::fs::flock(&lock, FlockOperation::LockShared) {
        eprintln!("onelf-rt: cannot lock {}: {e}", lock_path.display());
        return None;
    }

    let path = base.join(&dir_name);
    if let Err(e) = std::fs::create_dir_all(&path) {
        eprintln!("onelf-rt: cannot create {}: {e}", path.display());
        return None;
    }
    Some(Mountpoint { path, _lock: lock })
}

/// Where the launch resolver keeps its link farm and recorded decision for
/// the package `package_id`. Outside the `onelf-` namespace on purpose:
/// it must survive across launches, so the mountpoint sweep is not to
/// touch it. It lives and dies with the private dir.
pub fn resolve_store(package_id: &[u8; 32]) -> Option<PathBuf> {
    let base = private_dir()?;
    Some(base.join(format!("resolve-{}", crate::cache::hex(&package_id[0..8]))))
}

/// How long an unclaimed leftover must have sat untouched before a sweep may
/// reclaim it.
///
/// Two cases need it. A mountpoint directory with no lock file was written by
/// a release predating the locking protocol, so there is no claim to test and
/// age is the only thing standing between the sweep and a live mount. An
/// orphaned lock file whose directory is gone is only removable once no
/// instance could still be between opening it and creating its directory.
const RECLAIM_GRACE_SECS: u64 = 3600;

/// True when `path` has not been modified within [`RECLAIM_GRACE_SECS`].
/// Anything we cannot stat is treated as too young to touch.
fn older_than_grace(path: &Path) -> bool {
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return false;
    };
    let Ok(modified) = md.modified() else {
        return false;
    };
    modified
        .elapsed()
        .map(|age| age.as_secs() >= RECLAIM_GRACE_SECS)
        .unwrap_or(false)
}

/// Reclaim mountpoint directories, and the lock files of long-gone ones, that
/// no running instance has claimed.
///
/// A live mountpoint cannot be recognized by inspection: both mount modes
/// mount inside a private namespace, so from any other process the directory
/// is simply empty, exactly like an abandoned one. Ownership is therefore
/// proven by the lock the owner holds, not inferred from the directory.
pub fn sweep_stale_mountpoints() {
    // Only sweep our own 0700 per-uid base, never shared /tmp (where a
    // rmdir could touch another user's planted `onelf-*` dir).
    let Some(base) = private_dir() else {
        return;
    };
    sweep_in(&base);
}

/// [`sweep_stale_mountpoints`] against an explicit base, so the reclamation
/// rules can be exercised without depending on the process's private dir.
fn sweep_in(base: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        match name
            .strip_prefix('.')
            .and_then(|rest| rest.strip_suffix(".lock"))
        {
            Some(owner) if owner.starts_with("onelf-") => reclaim_orphan_lock(base, owner),
            _ if name.starts_with("onelf-") => reclaim_mountpoint(base, name, &entry.path()),
            _ => {}
        }
    }
}

/// Remove `path` when no instance claims the mountpoint directory it holds.
fn reclaim_mountpoint(base: &Path, dir_name: &str, path: &Path) {
    use rustix::fs::FlockOperation;

    if !path.is_dir() {
        return;
    }
    match std::fs::File::open(mountpoint_lock_path(base, dir_name)) {
        Ok(lock) => {
            if rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive).is_err() {
                return;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !older_than_grace(path) {
                return;
            }
        }
        Err(_) => return,
    }
    // rmdir fails atomically on a non-empty directory.
    let _ = std::fs::remove_dir(path);
}

/// Remove the lock file for `dir_name` once its directory is gone and nobody
/// holds it. Every distinct package id mints one, so without this they
/// accumulate for the life of the private dir.
///
/// Unlinking a lock is normally unsafe, because the protocol keys mutual
/// exclusion on the inode and a concurrent run could create a fresh one. The
/// grace period is what makes it safe here: reaching this point requires an
/// instance to have opened the lock and then not created its directory for an
/// hour, which the few statements between those two steps cannot produce.
fn reclaim_orphan_lock(base: &Path, dir_name: &str) {
    use rustix::fs::FlockOperation;

    if base.join(dir_name).exists() {
        return;
    }
    let lock_path = mountpoint_lock_path(base, dir_name);
    if !older_than_grace(&lock_path) {
        return;
    }
    let Ok(lock) = std::fs::File::open(&lock_path) else {
        return;
    };
    if rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive).is_err() {
        return;
    }
    let _ = std::fs::remove_file(&lock_path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn prefers_safe_xdg_runtime_dir() {
        // A 0700 dir we own is accepted as the private base.
        let dir = std::env::temp_dir().join(format!("onelf-priv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, PermissionsExt::from_mode(0o700)).unwrap();
        assert!(is_safe_owned_dir(&dir));

        // A world-accessible dir is rejected (would fall through / refuse).
        std::fs::set_permissions(&dir, PermissionsExt::from_mode(0o755)).unwrap();
        assert!(!is_safe_owned_dir(&dir));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A live instance's mountpoint must survive a concurrent launch's
    /// sweep. Before the lock protocol, both mount modes mounted inside a
    /// private namespace, so a live mountpoint looked exactly like an
    /// abandoned one from outside and got rmdir'd out from under the app.
    #[test]
    fn sweep_spares_a_claimed_mountpoint() {
        use rustix::fs::FlockOperation;

        let base = std::env::temp_dir().join(format!("onelf-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let claimed = base.join("onelf-held-aaaa");
        let idle = base.join("onelf-idle-bbbb");
        for d in [&claimed, &idle] {
            std::fs::create_dir_all(d).unwrap();
        }

        let held = open_lock_inheritable(&mountpoint_lock_path(&base, "onelf-held-aaaa")).unwrap();
        rustix::fs::flock(&held, FlockOperation::LockShared).unwrap();
        drop(open_lock_inheritable(&mountpoint_lock_path(&base, "onelf-idle-bbbb")).unwrap());

        sweep_in(&base);

        assert!(
            claimed.is_dir(),
            "a claimed mountpoint must survive a sweep"
        );
        assert!(!idle.exists(), "an unclaimed mountpoint must be reclaimed");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A directory from a release that predates the lock protocol has no
    /// claim to check, so it is spared until it has been idle a while.
    #[test]
    fn sweep_spares_a_recent_lockless_mountpoint() {
        let base = std::env::temp_dir().join(format!("onelf-sweep-old-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let recent = base.join("onelf-recent-cccc");
        std::fs::create_dir_all(&recent).unwrap();

        sweep_in(&base);
        assert!(
            recent.is_dir(),
            "a lockless mountpoint inside the grace period must survive"
        );

        let old = std::fs::FileTimes::new().set_modified(
            std::time::SystemTime::now() - std::time::Duration::from_secs(RECLAIM_GRACE_SECS * 2),
        );
        std::fs::File::options()
            .write(true)
            .open(&recent)
            .or_else(|_| std::fs::File::open(&recent))
            .and_then(|f| f.set_times(old))
            .unwrap();

        sweep_in(&base);
        assert!(!recent.exists(), "an aged lockless mountpoint is reclaimed");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Every distinct package id mints a lock file, so an orphan whose
    /// directory is gone must eventually be reclaimed or the private dir
    /// grows without bound.
    #[test]
    fn sweep_reclaims_orphan_lock_files() {
        let base = std::env::temp_dir().join(format!("onelf-orphan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let orphan = mountpoint_lock_path(&base, "onelf-gone-dddd");
        drop(open_lock_inheritable(&orphan).unwrap());

        sweep_in(&base);
        assert!(orphan.exists(), "a fresh orphan is inside the grace period");

        let aged = std::fs::FileTimes::new().set_modified(
            std::time::SystemTime::now() - std::time::Duration::from_secs(RECLAIM_GRACE_SECS * 2),
        );
        std::fs::File::options()
            .write(true)
            .open(&orphan)
            .unwrap()
            .set_times(aged)
            .unwrap();

        sweep_in(&base);
        assert!(!orphan.exists(), "an aged orphan lock is reclaimed");

        // A claimed lock is kept at any age, along with its directory.
        let live_dir = base.join("onelf-live-eeee");
        std::fs::create_dir_all(&live_dir).unwrap();
        let live = mountpoint_lock_path(&base, "onelf-live-eeee");
        let held = open_lock_inheritable(&live).unwrap();
        rustix::fs::flock(&held, rustix::fs::FlockOperation::LockShared).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&live)
            .unwrap()
            .set_times(aged)
            .unwrap();

        sweep_in(&base);
        assert!(live_dir.is_dir(), "a claimed mountpoint is kept");
        assert!(live.exists(), "a claimed lock is kept");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rejects_non_directory_and_missing() {
        assert!(!is_safe_owned_dir(std::path::Path::new(
            "/nonexistent/onelf/private/xyz"
        )));
    }
}

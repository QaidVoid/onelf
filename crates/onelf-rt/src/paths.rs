//! Private per-user scratch directory resolution.
//!
//! Runtime scratch state (PT_INTERP symlinks, FUSE/ephemeral mountpoints,
//! and the cache fallback) must never live at a shared, world-writable
//! path like `/tmp/onelf`, where another local user could pre-create or
//! tamper with entries. [`private_dir`] returns a `0700`, current-uid-owned
//! base instead. A privileged launch refuses every environment-named
//! candidate outright: `XDG_RUNTIME_DIR` and the HOME/XDG-derived cache
//! roots were chosen by the unprivileged caller that started the launch,
//! so root keeps only its own base, `/run/onelf` when `/run` is root's
//! alone and `/tmp/onelf-0` otherwise, both validated as root-owned
//! `0700` directories.

use std::path::{Path, PathBuf};

/// Current real uid, the target user the runtime's state is owned by.
pub fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// The private base directory for `uid`, or `None` when no safe location
/// can be established.
///
/// `xdg_runtime_dir` is the environment's suggestion, accepted only when
/// it is a real directory `uid` owns with no group or other access
/// (systemd already makes it `0700` per-uid). Otherwise the decision falls
/// to `<tmp>/onelf-<uid>`, created atomically at `0700` and confirmed
/// through `symlink_metadata` to be a real directory owned by `uid`. A
/// pre-existing path that is a symlink or owned by another user is
/// refused (never chmod'd through), so the caller fails closed.
///
/// A privileged uid refuses the suggestion outright: the value was left
/// behind by the unprivileged caller that started the launch, so no
/// root-owned state is ever named by the environment, however safe the
/// named directory looks. It takes the
/// [`privileged_base_in`](onelf_format::cache_layout::privileged_base_in)
/// decision over `run` and `tmp` instead.
fn private_dir_for(
    uid: u32,
    xdg_runtime_dir: Option<&std::ffi::OsStr>,
    run: &Path,
    tmp: &Path,
) -> Option<PathBuf> {
    use onelf_format::cache_layout as layout;

    if uid == 0 {
        return layout::privileged_base_in(uid, run, tmp);
    }
    if let Some(dir) = xdg_runtime_dir
        && layout::is_safe_owned_dir(Path::new(dir), uid)
    {
        return Some(PathBuf::from(dir));
    }
    let p = tmp.join(format!("onelf-{uid}"));
    layout::create_private_dir(&p, uid).then_some(p)
}

/// Return a `0700`, current-uid-owned scratch base directory, or `None`
/// when no safe location can be established. See [`private_dir_for`] for
/// the decision and its refusals.
pub fn private_dir() -> Option<PathBuf> {
    private_dir_for(
        uid(),
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
        Path::new("/run"),
        Path::new("/tmp"),
    )
}

/// Open a lock file leaving the descriptor inheritable (no `CLOEXEC`), so a
/// lock taken before an `exec` still guards the resource afterwards. `std`'s
/// `File::open` sets `CLOEXEC`, which would drop the claim at exec time.
pub fn open_lock_inheritable(path: &Path) -> std::io::Result<std::fs::File> {
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
    pub(crate) path: PathBuf,
    _lock: std::fs::File,
}

impl Mountpoint {
    /// The claimed directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Let go of the claim and remove the directory, unless another
    /// instance still claims it.
    ///
    /// The directory is shared by every launch of the package, and in
    /// another instance's mount namespace it may be that instance's
    /// mountpoint: removing it from here detaches that mount there too.
    /// Only the exclusive lock the sweep also takes proves nobody else is
    /// using it. A launch that still holds this claim through an inherited
    /// descriptor keeps the directory as well, and the sweep removes it
    /// once that launch is gone.
    pub fn release(self) {
        let Mountpoint { path, _lock } = self;
        drop(_lock);
        let (Some(base), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
        else {
            return;
        };
        let Ok(lock) = std::fs::File::open(mountpoint_lock_path(base, name)) else {
            return;
        };
        if rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok() {
            let _ = std::fs::remove_dir(&path);
        }
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
pub(crate) fn create_mountpoint(package_name: &str, package_id: &[u8; 32]) -> Option<Mountpoint> {
    let (path, lock) = claim_mountpoint(&package_dir_name(package_name, package_id))?;
    Some(Mountpoint { path, _lock: lock })
}

/// Create a directory for this launch alone, claimed the way
/// [`create_mountpoint`] claims a shared one and named like it with the
/// process id added.
///
/// A tree that is edited in place cannot be shared. Two launches of one
/// package would extract over each other's files, and one would write
/// through the symlink the other put over a bundled library, into the
/// host file it names.
pub fn create_launch_dir(package_name: &str, package_id: &[u8; 32]) -> Option<Mountpoint> {
    let dir_name = launch_dir_name(package_name, package_id, std::process::id());
    let (path, lock) = claim_mountpoint(&dir_name)?;
    Some(Mountpoint { path, _lock: lock })
}

/// Create the staging tree this launch fills before publishing its
/// runtime-directory view, claimed the way [`create_launch_dir`] claims
/// the view itself.
///
/// The name carries the launch directory's name, so the staging tree
/// belongs to one launch and is never confused with a published tree. It
/// keeps the `onelf-` prefix, so a launch killed mid-build leaves a
/// staging tree the next launch's sweep reclaims through its lock, like
/// any other claimed directory.
pub(crate) fn create_launch_stage(package_name: &str, package_id: &[u8; 32]) -> Option<Mountpoint> {
    let dir_name = format!(
        "{}.staging",
        launch_dir_name(package_name, package_id, std::process::id())
    );
    let (path, lock) = claim_mountpoint(&dir_name)?;
    Some(Mountpoint { path, _lock: lock })
}

/// The mountpoint concurrent launches of a package share (see
/// `fuse::share`), claimed the way [`create_mountpoint`] claims one.
pub(crate) fn create_shared_mountpoint(dir_name: &str) -> Option<Mountpoint> {
    let (path, lock) = claim_mountpoint(dir_name)?;
    Some(Mountpoint { path, _lock: lock })
}

/// `onelf-<name prefix>-<longer id prefix>`, the shared mountpoint's
/// name. It carries more of the id than [`package_dir_name`], so it is
/// never the directory a launch that does not share mounts at.
pub(crate) fn shared_dir_name(package_name: &str, package_id: &[u8; 32]) -> String {
    let hash_suffix = crate::cache::hex(&package_id[0..8]);
    format!("onelf-{}-{hash_suffix}", name_stem(package_name))
}

/// The record and the lease of the shared mountpoint `dir_name` under
/// `base`. The record names the server and is locked by every launch
/// deciding whether to start one. The lease is held shared by every
/// launch running from the mount.
pub(crate) fn share_files_in(base: &Path, dir_name: &str) -> (PathBuf, PathBuf) {
    (
        base.join(format!(".{dir_name}.share")),
        base.join(format!(".{dir_name}.lease")),
    )
}

/// [`share_files_in`] under the private runtime directory.
pub(crate) fn share_files(dir_name: &str) -> Option<(PathBuf, PathBuf)> {
    Some(share_files_in(&private_dir()?, dir_name))
}

/// `onelf-<name prefix>-<id prefix>`, shared by every launch of a package.
fn package_dir_name(package_name: &str, package_id: &[u8; 32]) -> String {
    let hash_suffix = crate::cache::hex(&package_id[0..4]);
    format!("onelf-{}-{hash_suffix}", name_stem(package_name))
}

/// The package's directory name with `pid` added, for one launch alone.
fn launch_dir_name(package_name: &str, package_id: &[u8; 32], pid: u32) -> String {
    format!("{}-{pid}", package_dir_name(package_name, package_id))
}

/// The mountpoint a dependency set's build is served at, named by the
/// build's hash so every package that pins that set shares one mount,
/// with the shared lock claiming it.
pub(crate) fn create_set_mountpoint(hash: &str) -> Option<(PathBuf, std::fs::File)> {
    claim_mountpoint(&format!("onelf-set-{}", &hash[..hash.len().min(16)]))
}

/// The lock file guarding the mountpoint named `dir_name`, for a server
/// that has to ask whether anyone still holds it.
pub(crate) fn set_lock_path(dir_name: &str) -> Option<PathBuf> {
    Some(mountpoint_lock_path(&private_dir()?, dir_name))
}

/// Take the lock that serializes creating the mount for `dir_name`.
///
/// The mountpoint's own lock cannot do this: every user holds it shared
/// for as long as it runs, so it says nothing about who is mounting. Two
/// launches starting together would otherwise both find no mount and
/// both make one, stacking a second mount on the first and leaving a
/// server with nothing to serve.
pub(crate) fn lock_set_creation(dir_name: &str) -> Option<std::fs::File> {
    let path = private_dir()?.join(format!(".{dir_name}.create"));
    let lock = open_lock_inheritable(&path).ok()?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive).ok()?;
    Some(lock)
}

/// Claim `dir_name` under the private runtime dir and create it. The
/// shared lock is taken first, so a concurrent sweep cannot mistake a
/// directory that exists but is not yet mounted for an abandoned one.
fn claim_mountpoint(dir_name: &str) -> Option<(PathBuf, std::fs::File)> {
    claim_in(&private_dir()?, dir_name)
}

/// [`claim_mountpoint`] under an explicit base.
fn claim_in(base: &Path, dir_name: &str) -> Option<(PathBuf, std::fs::File)> {
    use rustix::fs::FlockOperation;

    let base = base.to_path_buf();
    let lock_path = mountpoint_lock_path(&base, dir_name);
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

    let path = base.join(dir_name);
    if let Err(e) = std::fs::create_dir_all(&path) {
        eprintln!("onelf-rt: cannot create {}: {e}", path.display());
        return None;
    }
    Some((path, lock))
}

/// The package name as the readable part of a directory name: its first
/// six characters that are safe in one path component, or `pkg` when it
/// has none. The name is the manifest's, so only its length is checked
/// on the way in; a `/` in it would put the lock under a missing parent.
fn name_stem(package_name: &str) -> String {
    let stem: String = package_name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(6)
        .collect();
    if stem.is_empty() {
        "pkg".to_string()
    } else {
        stem
    }
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
pub(crate) fn sweep_stale_mountpoints() {
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
        let hidden = |suffix: &str| {
            name.strip_prefix('.')
                .and_then(|rest| rest.strip_suffix(suffix))
                .filter(|owner| owner.starts_with("onelf-"))
        };
        if let Some(owner) = hidden(".lock") {
            reclaim_orphan_lock(base, owner);
        } else if let Some(owner) = hidden(".share") {
            crate::fuse::share::reclaim_stale(base, owner);
        } else if name.starts_with("onelf-") {
            reclaim_mountpoint(base, name, &entry.path());
        }
    }
}

/// Remove `path` when no instance claims the mountpoint directory it holds.
///
/// A mount served through the host's FUSE helper lives in the host
/// namespace, so it outlives an owner that was killed. Its lock is free,
/// which is the only safe signal to go by: a mount whose daemon is alive
/// but slow would block a status query, and such a daemon holds the lock.
/// Once the lock is ours, a disconnected transport proves the daemon is
/// gone, and the mount is lazily unmounted before the directory goes.
fn reclaim_mountpoint(base: &Path, dir_name: &str, path: &Path) {
    use rustix::fs::FlockOperation;

    // The lock is opened before anything stats `path`: on a dead mount a
    // stat fails, and on a live private one it says nothing useful.
    let _lock = match std::fs::File::open(mountpoint_lock_path(base, dir_name)) {
        Ok(lock) => {
            if rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive).is_err() {
                return;
            }
            Some(lock)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !path.is_dir() || !older_than_grace(path) {
                return;
            }
            None
        }
        Err(_) => return,
    };
    if is_mountpoint(path) {
        if !is_dead_mount(path) {
            return;
        }
        crate::fuse::mount::fuse_unmount(path);
    }
    match _lock {
        // A claimed directory whose claim lapsed may hold a whole extracted
        // tree, left by an instance that was killed.
        Some(_) => {
            let _ = std::fs::remove_dir_all(path);
        }
        // Without a claim to test, rmdir is as far as it is safe to go: it
        // fails atomically on anything that is not empty.
        None => {
            let _ = std::fs::remove_dir(path);
        }
    }
}

/// Whether `path` is a mountpoint in this process's mount namespace,
/// read from `/proc/self/mountinfo` so nothing has to stat a mount that
/// may no longer answer.
pub(crate) fn is_mountpoint(path: &Path) -> bool {
    let target = path.to_string_lossy();
    let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    info.lines().any(|line| {
        line.split(' ')
            .nth(4)
            .map(|mp| mp.replace("\\040", " ") == *target)
            .unwrap_or(false)
    })
}

/// Whether the filesystem at `path` reports a disconnected transport, which
/// is what a FUSE mount answers once its daemon has exited. The kernel
/// fails the query immediately in that state, so this never blocks.
fn is_dead_mount(path: &Path) -> bool {
    matches!(rustix::fs::statfs(path), Err(rustix::io::Errno::NOTCONN))
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
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    /// A scratch fallback root standing in for `/tmp`, so a simulated
    /// root's per-uid directory never touches the real one.
    fn fallback_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("onelf-pbase-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn prefers_safe_xdg_runtime_dir() {
        let fb = fallback_root("prefers");
        let dir = fb.join("xdg");

        // A 0700 dir we own is accepted as the private base.
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        assert_eq!(
            private_dir_for(uid(), Some(dir.as_os_str()), &fb, &fb),
            Some(dir.clone())
        );

        // A world-accessible dir is rejected in favour of the per-uid one.
        std::fs::set_permissions(&dir, PermissionsExt::from_mode(0o755)).unwrap();
        let fell = private_dir_for(uid(), Some(dir.as_os_str()), &fb, &fb).unwrap();
        assert_ne!(fell, dir);
        assert_eq!(fell, fb.join(format!("onelf-{}", uid())));

        let _ = std::fs::remove_dir_all(&fb);
    }

    /// The per-uid fallback stays at the mode it is created with: no
    /// group or other access, whatever the umask and the caller's
    /// permissions on the parent are.
    #[test]
    fn the_fallback_base_is_created_private() {
        let fb = fallback_root("mode");
        let got = private_dir_for(uid(), None, &fb, &fb).unwrap();
        let mode = std::fs::symlink_metadata(&got)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        let _ = std::fs::remove_dir_all(&fb);
    }

    /// A privileged uid refuses the environment's base however safe it
    /// looks, because the value was chosen by the unprivileged caller.
    /// Its per-uid directory decides instead; here that directory is
    /// user-owned, so on a non-root run the decision fails closed. The
    /// two outcomes are identical with and without the env value, which
    /// is the refusal itself.
    #[test]
    fn root_refuses_the_env_base() {
        let fb = fallback_root("root");
        let xdg = fb.join("xdg");
        std::fs::DirBuilder::new().mode(0o700).create(&xdg).unwrap();

        let run = fb.join("run");
        let with_env = private_dir_for(0, Some(xdg.as_os_str()), &run, &fb);
        let without_env = private_dir_for(0, None, &run, &fb);
        assert_eq!(
            with_env, without_env,
            "the env value must not change root's base"
        );
        if uid() != 0 {
            assert_eq!(
                with_env, None,
                "a user-owned per-uid base is refused for root"
            );
        }
        assert!(xdg.is_dir(), "the refused candidate is left untouched");

        let _ = std::fs::remove_dir_all(&fb);
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
    fn sweep_leaves_unprefixed_dirs_alone() {
        // `modules-*` (rewritten caches) and `paths-*` (overlay staging)
        // deliberately lack the `onelf-` prefix the sweep treats as a
        // mountpoint, so live state is never reclaimed.
        let base = std::env::temp_dir().join(format!("onelf-sweep-keep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let modules = base.join("modules-pkg-abcdef01");
        let staging = base.join("paths-424242");
        std::fs::create_dir_all(modules.join("sub")).unwrap();
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(modules.join("sub/loaders.cache"), b"x").unwrap();

        sweep_in(&base);

        assert!(modules.join("sub/loaders.cache").is_file());
        assert!(staging.is_dir());

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Every instance of a package claims the same directory, and in
    /// another instance's namespace it is that instance's mountpoint. The
    /// first to exit removed it regardless, which detached the mount the
    /// other was still running from.
    #[test]
    fn a_released_mountpoint_stays_while_another_instance_claims_it() {
        let _locking = crate::test_locks::locking();
        let base = std::env::temp_dir().join(format!("onelf-release-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let claim = |name: &str| {
            let (path, lock) = claim_in(&base, name).unwrap();
            Mountpoint { path, _lock: lock }
        };
        let first = claim("onelf-pkg-aaaa");
        let second = claim("onelf-pkg-aaaa");
        let dir = first.path().to_path_buf();

        second.release();
        assert!(dir.is_dir(), "the first instance still claims it");
        first.release();
        assert!(!dir.exists(), "the last claim removes it");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn concurrent_launches_get_trees_of_their_own() {
        let id = [7u8; 32];
        assert_ne!(
            launch_dir_name("app", &id, 100),
            launch_dir_name("app", &id, 101)
        );
        assert!(launch_dir_name("app", &id, 100).starts_with(&package_dir_name("app", &id)));
    }

    #[test]
    fn rejects_non_directory_and_missing() {
        let safe = onelf_format::cache_layout::is_safe_owned_dir;
        assert!(!safe(Path::new("/nonexistent/onelf/private/xyz"), uid()));
    }
}

#[cfg(test)]
mod name_stem_tests {
    use super::name_stem;

    #[test]
    fn keeps_only_path_safe_characters() {
        assert_eq!(name_stem("my app/\u{e9}vil-1.2"), "myappv");
        assert_eq!(name_stem("blender"), "blende");
        assert_eq!(name_stem("///"), "pkg");
        assert_eq!(name_stem(""), "pkg");
    }
}

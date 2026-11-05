//! Shared layout and safety rules for the on-disk extraction cache.
//!
//! The runtime and the packer CLI both read and write this cache. They used
//! to answer "where is it, and is it safe to use" independently, and drifted:
//! one required an absolute env-derived path and a `0700` uid-owned
//! directory, the other happily fell back to a shared `/tmp`. Both now go
//! through here.
//!
//! The current user id is a parameter rather than a call, so this crate stays
//! dependency-free; each caller already has a way to ask the OS.
//!
//! Layout under the resolved root:
//!
//! ```text
//! pkg/<id>              extracted package tree
//! pkg/.<id>.ready       completion marker, written only after a full extract
//! pkg/.<id>.tmp         extraction scratch, renamed to pkg/<id> on success
//! cas/<aa>/<hash>       content blob, hardlinked into package trees
//! meta/<id>             last-used timestamp
//! lock/<id>             in-use lock, held shared for an instance's lifetime
//! lock/<id>.extract     extraction mutex
//! loaders/<hash>/<name> closed host loader copy, keyed by its BLAKE3
//! ```

use std::path::{Path, PathBuf};

use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

/// True if `path` is a real directory (not a symlink) owned by `uid` with no
/// group or other permission bits. Uses `symlink_metadata`, so a planted
/// symlink is rejected rather than followed.
pub fn is_safe_owned_dir(path: &Path, uid: u32) -> bool {
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return false;
    };
    md.is_dir() && md.uid() == uid && (md.mode() & 0o077) == 0
}

/// Ensure `dir` is a `0700` directory owned by `uid`, creating it atomically
/// at that mode when absent. Returns false if it exists as a symlink or as
/// another user's directory, which are never chmod'd through; a real
/// directory we own but with loose permissions is tightened in place.
pub fn ensure_safe_dir(dir: &Path, uid: u32) -> bool {
    if let Some(parent) = dir.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => return true,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return false,
    }
    if let Ok(md) = std::fs::symlink_metadata(dir)
        && md.is_dir()
        && md.uid() == uid
        && md.mode() & 0o077 != 0
    {
        let _ = std::fs::set_permissions(dir, PermissionsExt::from_mode(0o700));
    }
    is_safe_owned_dir(dir, uid)
}

/// The root-owned private base a privileged process keeps its state
/// under, chosen without consulting the environment: `/run/onelf` when
/// `/run` is a real root-owned directory nobody else may write, else
/// `/tmp/onelf-0`. See [`privileged_base_in`] for the acceptance rules.
pub fn privileged_base() -> Option<PathBuf> {
    privileged_base_in(0, Path::new("/run"), Path::new("/tmp"))
}

/// [`privileged_base`]'s rules for `uid` over explicit candidate parents.
///
/// `<run>/onelf` is preferred when `run` is a real directory `uid` owns
/// that group and other cannot write, so nobody else can have planted
/// the name. Otherwise `<tmp>/onelf-<uid>` serves, which any user may
/// pre-create in a sticky `/tmp`. Either candidate is created `0700` when
/// absent and accepted only as a real directory owned by `uid` with no
/// group or other access; a pre-existing one that fails is refused, never
/// chmod'd through.
pub fn privileged_base_in(uid: u32, run: &Path, tmp: &Path) -> Option<PathBuf> {
    let run_is_private = std::fs::symlink_metadata(run)
        .is_ok_and(|md| md.is_dir() && md.uid() == uid && md.mode() & 0o022 == 0);
    if run_is_private {
        let dir = run.join("onelf");
        if create_private_dir(&dir, uid) {
            return Some(dir);
        }
    }
    let dir = tmp.join(format!("onelf-{uid}"));
    create_private_dir(&dir, uid).then_some(dir)
}

/// The persistent root-owned base a privileged caller with no private
/// base of its own keeps its cache under: `/var/cache`, when it is a
/// real directory root owns that group and other cannot write. Unlike
/// [`privileged_base`], it survives a reboot, which is what the packer
/// CLI's fetched builds and resolver state need. See
/// [`persistent_base_in`] for the acceptance rule.
fn persistent_privileged_base() -> Option<PathBuf> {
    persistent_base_in(0, Path::new("/var/cache"))
}

/// The persistent cache root a privileged process may keep state under
/// that has to survive a reboot and be executable, `/var/cache/onelf`,
/// when `/var/cache` is a real directory root owns that group and other
/// cannot write. Nothing is created.
pub fn persistent_privileged_root() -> Option<PathBuf> {
    persistent_privileged_base().map(|base| base.join("onelf"))
}

/// [`persistent_privileged_base`]'s rule for `uid` over an explicit
/// candidate: `var_cache` is accepted only as a real directory `uid` owns
/// that group and other cannot write, so nobody else can have planted
/// the `onelf` name the cache root takes under it.
fn persistent_base_in(uid: u32, var_cache: &Path) -> Option<PathBuf> {
    std::fs::symlink_metadata(var_cache)
        .is_ok_and(|md| md.is_dir() && md.uid() == uid && md.mode() & 0o022 == 0)
        .then(|| var_cache.to_path_buf())
}

/// Create `dir` at `0700` with one `mkdir`, so it is never group or
/// other accessible, and report whether it is now a real directory `uid`
/// owns with no group or other access. A pre-existing path is checked,
/// never chmod'd, since it may be a symlink or another user's.
pub fn create_private_dir(dir: &Path, uid: u32) -> bool {
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return false,
    }
    is_safe_owned_dir(dir, uid)
}

/// The cache base the environment names for `uid`: `$XDG_CACHE_HOME`,
/// then `$HOME/.cache`, each only when absolute, because an empty or
/// relative value would resolve against the current directory. A
/// privileged uid takes none: both variables were chosen by the
/// unprivileged caller that started the launch, and `sudo -E` keeps them
/// pointing into that caller's home.
fn env_base(uid: u32, var: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    if uid == 0 {
        return None;
    }
    let abs = |v: std::ffi::OsString| -> Option<PathBuf> {
        let p = PathBuf::from(v);
        p.is_absolute().then_some(p)
    };
    var("XDG_CACHE_HOME")
        .and_then(abs)
        .or_else(|| var("HOME").and_then(abs).map(|h| h.join(".cache")))
}

/// Resolve the cache root, or `None` when no safe location can be
/// established. Callers MUST treat `None` as a refusal rather than
/// substituting a guess: a shared world-writable fallback is what this
/// function exists to prevent.
///
/// An ordinary uid prefers `$XDG_CACHE_HOME`, then `~/.cache`, both of
/// which are already user-private, and only an absolute value is
/// trusted. `fallback` supplies the last resort (the runtime passes its
/// `0700` per-uid directory; a caller with none passes `None`). Uid 0
/// ignores the environment entirely and uses `fallback`. A caller with
/// none, the packer CLI, gets the persistent `/var/cache/onelf`, or the
/// tmpfs-backed [`privileged_base`] when `/var/cache` is unusable, so
/// root never creates or trusts state under a path the invoking user
/// chose.
pub fn resolve_root(uid: u32, fallback: Option<PathBuf>) -> Option<PathBuf> {
    resolve_root_with(uid, |name| std::env::var_os(name), fallback)
}

/// [`resolve_root`] over an injected environment lookup.
fn resolve_root_with(
    uid: u32,
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
    fallback: Option<PathBuf>,
) -> Option<PathBuf> {
    let base = env_base(uid, var).or(fallback).or_else(|| {
        (uid == 0)
            .then(|| persistent_privileged_base().or_else(privileged_base))
            .flatten()
    })?;
    let onelf = base.join("onelf");
    ensure_safe_dir(&onelf, uid).then_some(onelf)
}

/// Where [`resolve_root`] would place the cache root for `uid`, computed
/// from the environment alone: nothing is created and nothing on disk is
/// probed. The launch export answers this so publishing where the cache
/// is never establishes it. A launch that runs a closed host loader does
/// create the root, to keep the copy in; acquisition keeps using
/// [`resolve_root`], which creates the root under its safe-creation
/// rules on demand. The `fallback` applies only when the environment
/// names no absolute cache location, and is the only base uid 0 takes.
pub fn proposed_root(uid: u32, fallback: Option<PathBuf>) -> Option<PathBuf> {
    proposed_root_with(uid, |name| std::env::var_os(name), fallback)
}

/// [`proposed_root`] over an injected environment lookup.
fn proposed_root_with(
    uid: u32,
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
    fallback: Option<PathBuf>,
) -> Option<PathBuf> {
    env_base(uid, var)
        .or(fallback)
        .map(|base| base.join("onelf"))
}

/// Extracted tree for `package_id`.
pub fn pkg_dir(root: &Path, package_id: &str) -> PathBuf {
    root.join("pkg").join(package_id)
}

/// Completion marker for `package_id`. Its presence is what distinguishes a
/// fully extracted tree from one an interrupted run left behind.
pub fn ready_marker(root: &Path, package_id: &str) -> PathBuf {
    root.join("pkg").join(format!(".{package_id}.ready"))
}

/// Extraction scratch for `package_id`, renamed to [`pkg_dir`] once the
/// tree is complete. An interrupted extraction leaves it behind.
pub fn extract_tmp_dir(root: &Path, package_id: &str) -> PathBuf {
    root.join("pkg").join(format!(".{package_id}.tmp"))
}

/// In-use lock for `package_id`, held shared for an instance's lifetime.
pub fn lock_path(root: &Path, package_id: &str) -> PathBuf {
    root.join("lock").join(package_id)
}

/// Extraction mutex for `package_id`, distinct from the in-use lock so a
/// second runner waits only for extraction, not for the first instance's
/// whole lifetime.
pub fn extract_lock_path(root: &Path, package_id: &str) -> PathBuf {
    root.join("lock").join(format!("{package_id}.extract"))
}

/// Last-used timestamp for `package_id`.
pub fn meta_path(root: &Path, package_id: &str) -> PathBuf {
    root.join("meta").join(package_id)
}

/// Store-wide lock separating extraction from collection.
///
/// Extraction holds it *shared* while it populates the content store, because
/// a blob is briefly reachable only from the store itself: it is renamed into
/// place before being hardlinked into the package tree, and in that window its
/// link count does not yet reflect the reference that is about to exist.
/// Collection holds it *exclusive*, which proves no extraction is inside that
/// window and therefore that link counts can be trusted.
/// The GL build store: one directory per label holding the pinned
/// build's file. Its extraction lives under `pkg/` like any package.
pub fn platform_dir(root: &Path) -> PathBuf {
    root.join("platform")
}

pub fn cas_lock_path(root: &Path) -> PathBuf {
    root.join("lock").join("cas")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Distinguishes scratch paths between concurrently running tests, which
    /// share a process and therefore a pid.
    fn unique(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "onelf-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// The uid that owns what this process creates, read back from a scratch
    /// file. The crate has no dependencies, so there is no `getuid` to call.
    fn uid() -> u32 {
        let p = unique("uidprobe");
        std::fs::write(&p, b"").unwrap();
        let u = std::fs::symlink_metadata(&p).unwrap().uid();
        let _ = std::fs::remove_file(&p);
        u
    }

    #[test]
    fn safe_dir_rules() {
        let me = uid();
        let dir = unique("layout");
        let _ = std::fs::remove_dir_all(&dir);

        assert!(ensure_safe_dir(&dir, me), "fresh dir is created 0700");
        assert!(is_safe_owned_dir(&dir, me));

        std::fs::set_permissions(&dir, PermissionsExt::from_mode(0o755)).unwrap();
        assert!(!is_safe_owned_dir(&dir, me), "0755 is not owner-private");
        assert!(ensure_safe_dir(&dir, me), "our own dir is tightened back");
        assert!(is_safe_owned_dir(&dir, me));

        assert!(
            !is_safe_owned_dir(&dir, me.wrapping_add(1)),
            "another uid's directory is refused, never chmod'd"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_and_non_directory_are_unsafe() {
        let me = uid();
        assert!(!is_safe_owned_dir(Path::new("/nonexistent/onelf/xyz"), me));

        let f = unique("layout-file");
        std::fs::write(&f, b"x").unwrap();
        assert!(!is_safe_owned_dir(&f, me), "a regular file is not a dir");
        let _ = std::fs::remove_file(&f);
    }

    /// A relative `XDG_CACHE_HOME` must be ignored in favour of the
    /// fallback, so the cache never lands under the current directory.
    ///
    /// This is the only test in the crate that touches the environment, and
    /// it restores both variables before returning.
    #[test]
    fn relative_env_roots_are_not_trusted() {
        let me = uid();
        let fallback = unique("fb");
        let _ = std::fs::remove_dir_all(&fallback);
        std::fs::create_dir_all(&fallback).unwrap();

        let prev_xdg = std::env::var_os("XDG_CACHE_HOME");
        let prev_home = std::env::var_os("HOME");
        // SAFETY: no other test in this binary reads the environment.
        unsafe {
            std::env::set_var("XDG_CACHE_HOME", "relative/path");
            std::env::remove_var("HOME");
        }

        let root = resolve_root(me, Some(fallback.clone()));

        unsafe {
            match prev_xdg {
                Some(v) => std::env::set_var("XDG_CACHE_HOME", v),
                None => std::env::remove_var("XDG_CACHE_HOME"),
            }
            if let Some(v) = prev_home {
                std::env::set_var("HOME", v);
            }
        }

        assert_eq!(root, Some(fallback.join("onelf")));
        let _ = std::fs::remove_dir_all(&fallback);
    }

    /// An environment lookup over fixed values, so the root decision is
    /// tested without touching the process environment.
    fn env_of(pairs: &[(&str, &Path)]) -> impl Fn(&str) -> Option<std::ffi::OsString> + use<> {
        let pairs: Vec<(String, PathBuf)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_path_buf()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone().into_os_string())
        }
    }

    /// Uid 0 takes neither `XDG_CACHE_HOME` nor `HOME`: the proposal is
    /// the private base the caller supplies, and resolving creates nothing
    /// under the directories the environment names. An ordinary uid keeps
    /// the environment's choice.
    #[test]
    fn root_ignores_the_environment_cache_base() {
        let xdg = unique("root-xdg");
        let home = unique("root-home");
        let fallback = unique("root-fb");
        for dir in [&xdg, &home, &fallback] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let both = env_of(&[("XDG_CACHE_HOME", &xdg), ("HOME", &home)]);
        let home_only = env_of(&[("HOME", &home)]);

        assert_eq!(
            proposed_root_with(0, &both, Some(fallback.clone())),
            Some(fallback.join("onelf"))
        );
        assert_eq!(proposed_root_with(0, &both, None), None);
        assert_eq!(
            proposed_root_with(1000, &both, Some(fallback.clone())),
            Some(xdg.join("onelf"))
        );
        assert_eq!(
            proposed_root_with(1000, &home_only, None),
            Some(home.join(".cache").join("onelf"))
        );

        let _ = resolve_root_with(0, &both, Some(fallback.clone()));
        assert_eq!(std::fs::read_dir(&xdg).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(&home).unwrap().count(), 0);

        for dir in [&xdg, &home, &fallback] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// The privileged base prefers the run directory only when its owner
    /// is the uid and nobody else may write it, and falls back to the
    /// per-uid temporary name, which is refused when another object
    /// already holds it.
    #[test]
    fn the_privileged_base_prefers_a_private_run_directory() {
        let me = uid();
        let top = unique("pbase");
        let run = top.join("run");
        let tmp = top.join("tmp");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::set_permissions(&run, PermissionsExt::from_mode(0o755)).unwrap();

        let got = privileged_base_in(me, &run, &tmp).unwrap();
        assert_eq!(got, run.join("onelf"));
        assert_eq!(
            std::fs::symlink_metadata(&got).unwrap().mode() & 0o777,
            0o700
        );

        std::fs::remove_dir(&got).unwrap();
        std::fs::set_permissions(&run, PermissionsExt::from_mode(0o777)).unwrap();
        assert_eq!(
            privileged_base_in(me, &run, &tmp),
            Some(tmp.join(format!("onelf-{me}"))),
            "a run directory others may write is never used"
        );
        assert!(!run.join("onelf").exists());

        std::fs::remove_dir(tmp.join(format!("onelf-{me}"))).unwrap();
        std::os::unix::fs::symlink(&top, tmp.join(format!("onelf-{me}"))).unwrap();
        assert_eq!(
            privileged_base_in(me, &top.join("absent"), &tmp),
            None,
            "a planted fallback name is refused"
        );

        let _ = std::fs::remove_dir_all(&top);
    }

    /// The persistent base is taken only when the candidate is a real
    /// directory the uid owns that nobody else may write: a writable or
    /// foreign directory, a symlink and a missing path are all refused.
    #[test]
    fn the_persistent_base_requires_a_private_owned_directory() {
        let me = uid();
        let top = unique("vbase");
        let cache = top.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::set_permissions(&cache, PermissionsExt::from_mode(0o755)).unwrap();
        assert_eq!(persistent_base_in(me, &cache), Some(cache.clone()));
        assert_eq!(persistent_base_in(me.wrapping_add(1), &cache), None);

        std::fs::set_permissions(&cache, PermissionsExt::from_mode(0o777)).unwrap();
        assert_eq!(persistent_base_in(me, &cache), None, "others may write it");

        let link = top.join("link");
        std::os::unix::fs::symlink(&cache, &link).unwrap();
        std::fs::set_permissions(&cache, PermissionsExt::from_mode(0o755)).unwrap();
        assert_eq!(persistent_base_in(me, &link), None, "a symlink is refused");
        assert_eq!(persistent_base_in(me, &top.join("absent")), None);

        let _ = std::fs::remove_dir_all(&top);
    }

    #[test]
    fn layout_paths_are_stable() {
        let root = Path::new("/c/onelf");
        assert_eq!(pkg_dir(root, "ab"), Path::new("/c/onelf/pkg/ab"));
        assert_eq!(
            ready_marker(root, "ab"),
            Path::new("/c/onelf/pkg/.ab.ready")
        );
        assert_eq!(lock_path(root, "ab"), Path::new("/c/onelf/lock/ab"));
        assert_eq!(
            extract_lock_path(root, "ab"),
            Path::new("/c/onelf/lock/ab.extract")
        );
        assert_eq!(meta_path(root, "ab"), Path::new("/c/onelf/meta/ab"));
    }
}

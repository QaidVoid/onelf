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

/// Resolve the cache root, or `None` when no safe location can be
/// established. Callers MUST treat `None` as a refusal rather than
/// substituting a guess: a shared world-writable fallback is what this
/// function exists to prevent.
///
/// Prefers `$XDG_CACHE_HOME`, then `~/.cache`, both of which are already
/// user-private. Only an *absolute* env-derived path is trusted, because an
/// empty or relative one would resolve against the current directory, which
/// the caller does not control. `fallback` supplies the last resort (the
/// runtime passes its `0700` per-uid directory; a caller with none passes
/// `None`).
pub fn resolve_root(uid: u32, fallback: Option<PathBuf>) -> Option<PathBuf> {
    let abs = |v: std::ffi::OsString| -> Option<PathBuf> {
        let p = PathBuf::from(v);
        p.is_absolute().then_some(p)
    };
    let base = std::env::var_os("XDG_CACHE_HOME")
        .and_then(abs)
        .or_else(|| {
            std::env::var_os("HOME")
                .and_then(abs)
                .map(|h| h.join(".cache"))
        })
        .or(fallback)?;
    let onelf = base.join("onelf");
    ensure_safe_dir(&onelf, uid).then_some(onelf)
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

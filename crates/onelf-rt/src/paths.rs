//! Private per-user scratch directory resolution.
//!
//! Runtime scratch state (PT_INTERP symlinks, FUSE/ephemeral mountpoints,
//! and the cache fallback) must never live at a shared, world-writable
//! path like `/tmp/onelf`, where another local user could pre-create or
//! tamper with entries. [`private_dir`] returns a `0700`, current-uid-owned
//! base instead.

use std::path::{Path, PathBuf};

use std::os::unix::fs::{DirBuilderExt, MetadataExt};

/// Current real uid.
fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// True if `path` is a real directory (not a symlink) owned by the current
/// uid with no group/other permission bits. Uses `symlink_metadata` so a
/// planted symlink is rejected rather than followed.
fn is_safe_owned_dir(path: &Path) -> bool {
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return false;
    };
    md.is_dir() && md.uid() == uid() && (md.mode() & 0o077) == 0
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

    #[test]
    fn rejects_non_directory_and_missing() {
        assert!(!is_safe_owned_dir(std::path::Path::new(
            "/nonexistent/onelf/private/xyz"
        )));
    }
}

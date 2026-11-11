//! Descriptor 77 on the package's anchor directory.
//!
//! A package whose bundled libraries were built to open an absolute
//! directory can carry those strings rewritten to `/dev/fd/77/<key>`
//! (see [`onelf_format::anchors`]). Before the entrypoint runs, the
//! runtime makes a directory of its own under the private runtime
//! directory, puts one symlink per key in it naming the bundle directory
//! the key stands for, and leaves descriptor 77 open on it across the
//! exec. The application and every child it starts resolve the anchored
//! paths through their own descriptor table, with or without a mount
//! namespace, and nothing outside the private runtime directory is
//! created or consulted.
//!
//! The directory is claimed like a mountpoint, with a shared lock the
//! application inherits, so a later launch's sweep removes it once the
//! last process holding the claim is gone and never while one runs.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use onelf_format::anchors::{self, ANCHOR_FD, ANCHORS_FILE, Anchor, MAX_RECORD_BYTES};

use crate::loader::{self, PackageData};
use crate::paths::Mountpoint;

/// The anchors the package records, read through the verified package
/// path. A package without the record has none. A record over its size
/// bound, failing its content hash or not decoding is an error, so the
/// caller can say why nothing is anchored.
pub fn read(pkg: &PackageData) -> io::Result<Vec<Anchor>> {
    let Some(bytes) = loader::read_bounded_record(pkg, ANCHORS_FILE, MAX_RECORD_BYTES)? else {
        return Ok(Vec::new());
    };
    anchors::decode(&bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{ANCHORS_FILE}: {e}")))
}

/// Keep descriptor 77 from being handed to anything the runtime opens on
/// its way to the launch. Called first, before the package is opened.
///
/// The kernel gives every new descriptor the lowest free number, so a
/// launch started with 0 to 76 open would put the package file, a lock or
/// a FUSE descriptor on 77, and [`hold`] would close it from under the
/// runtime. A placeholder taken now closes on exec, so a package without
/// anchors finds descriptor 77 as its caller left it: closed. A 77 the
/// caller left open is already taken and stays until [`hold`] replaces
/// it.
pub fn reserve() {
    // SAFETY: only borrowed for the query.
    let slot = unsafe { BorrowedFd::borrow_raw(ANCHOR_FD) };
    if rustix::io::fcntl_getfd(slot).is_ok() {
        return;
    }
    let Ok(placeholder) = rustix::fs::open(
        "/",
        rustix::fs::OFlags::PATH | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    ) else {
        return;
    };
    // With every lower descriptor taken, the open itself lands on 77.
    if placeholder.as_raw_fd() == ANCHOR_FD {
        std::mem::forget(placeholder);
        return;
    }
    // SAFETY: descriptor 77 is closed, so nothing owns it; the handle is
    // forgotten so the placeholder stays put until `hold` or the exec.
    let mut target = unsafe { OwnedFd::from_raw_fd(ANCHOR_FD) };
    let _ = rustix::io::dup3(&placeholder, &mut target, rustix::io::DupFlags::CLOEXEC);
    std::mem::forget(target);
}

/// Close descriptor 77, for a launch whose anchors could not be set up.
/// A 77 the caller left open would otherwise reach the application, and
/// an anchored path would resolve inside whatever it names instead of
/// finding nothing.
pub fn release_descriptor() {
    // SAFETY: only borrowed for the query.
    let slot = unsafe { BorrowedFd::borrow_raw(ANCHOR_FD) };
    if rustix::io::fcntl_getfd(slot).is_ok() {
        // SAFETY: the descriptor is open and is the anchor's to give up.
        drop(unsafe { OwnedFd::from_raw_fd(ANCHOR_FD) });
    }
}

/// Whether the package carries an anchor record at all, without reading
/// it. A mode that has no package tree cannot serve one.
pub fn carried(pkg: &PackageData) -> bool {
    loader::find_file_entry(pkg, ANCHORS_FILE).is_some()
}

/// Where the anchor for `anchor` points in a launch whose package root is
/// `pkg_root`: the merged view of the directory it stands for when one
/// stands, else the bundle directory itself, the same choice the mount
/// side makes.
fn link_target(pkg_root: &Path, anchor: &Anchor, views: &BTreeMap<PathBuf, PathBuf>) -> PathBuf {
    crate::hostpaths::mapped_or_bundled(views, pkg_root.join(&anchor.relative))
}

/// Leave descriptor 77 open on a fresh anchor directory for `anchors`,
/// whose keys link into the package at `pkg_root`.
///
/// Returns the claim on the directory, which the caller keeps until it
/// execs: its lock is inherited by the application and holds the
/// directory for as long as any process of it runs. No anchors make no
/// directory and leave descriptor 77 alone.
pub fn hold(
    pkg_root: &Path,
    anchors: &[Anchor],
    views: &BTreeMap<PathBuf, PathBuf>,
) -> io::Result<Option<Mountpoint>> {
    if anchors.is_empty() {
        return Ok(None);
    }
    let claim = crate::paths::create_anchor_dir()
        .ok_or_else(|| io::Error::other("no private runtime directory to keep the anchors in"))?;
    for anchor in anchors {
        std::os::unix::fs::symlink(
            link_target(pkg_root, anchor, views),
            claim.path().join(&anchor.key),
        )?;
    }
    let dir = rustix::fs::open(
        claim.path(),
        rustix::fs::OFlags::PATH | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?;
    if dir.as_raw_fd() == ANCHOR_FD {
        rustix::io::fcntl_setfd(&dir, rustix::io::FdFlags::empty())?;
        std::mem::forget(dir);
        return Ok(Some(claim));
    }
    // SAFETY: descriptor 77 is taken over whatever it holds, which
    // `dup2` closes; the handle is forgotten afterwards, so it is never
    // closed here and stays open across the exec.
    let mut slot = unsafe { OwnedFd::from_raw_fd(ANCHOR_FD) };
    // dup2 leaves the new descriptor without close-on-exec, which is
    // what lets the application inherit it.
    let duplicated = rustix::io::dup2(&dir, &mut slot);
    std::mem::forget(slot);
    duplicated?;
    Ok(Some(claim))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_anchor_links_to_its_bundle_directory() {
        let anchor = Anchor {
            key: "a".into(),
            relative: "share/alsa".into(),
        };
        assert_eq!(
            link_target(
                Path::new("/run/user/1000/onelf-app-1234"),
                &anchor,
                &BTreeMap::new()
            ),
            Path::new("/run/user/1000/onelf-app-1234/share/alsa")
        );
        let root = std::env::temp_dir().join(format!("onelf-anchor-link-{}", std::process::id()));
        let view = root.join("view");
        std::fs::create_dir_all(&view).unwrap();
        let mut views = BTreeMap::new();
        views.insert(
            PathBuf::from("/run/user/1000/onelf-app-1234/share/alsa"),
            view.clone(),
        );
        assert_eq!(
            link_target(Path::new("/run/user/1000/onelf-app-1234"), &anchor, &views),
            view,
            "a built view stands in for the bundled directory"
        );
        views.insert(
            PathBuf::from("/run/user/1000/onelf-app-1234/share/alsa"),
            root.join("gone"),
        );
        assert_eq!(
            link_target(Path::new("/run/user/1000/onelf-app-1234"), &anchor, &views),
            Path::new("/run/user/1000/onelf-app-1234/share/alsa"),
            "a vanished view falls back like the mount side"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

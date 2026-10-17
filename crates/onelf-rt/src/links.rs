//! Libraries from outside the package, shown inside its library directory.
//!
//! A launch can take libraries from two places besides the bundle: the
//! resolver's link farm, which holds the host libraries it chose and those
//! of a fetched GL build, and the dependency sets the package pins. Every
//! bundled executable carries a `$ORIGIN` run path naming the bundle's
//! library directory, and that run path is a `DT_RPATH`, which the loader
//! also searches for every library below the executable. A link placed in
//! that directory is therefore found the way a bundled library is, and no
//! `LD_LIBRARY_PATH` is needed. The variable would reach every process the
//! app spawns, and host programs would load the same libraries.
//!
//! Only names the package does not carry are added. A host library that
//! beat a bundled copy takes that copy's place by other means (see
//! `launch::shadow_bundled_copies`).
//!
//! A tree extracted for one launch, and the shared cache tree, get real
//! symlinks. A FUSE mount serves them as entries of its own, which the
//! server learns from the launching child through a [`channel`].

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use onelf_format::Manifest;
use onelf_format::entry::EntryKind;

/// Link name to target, ordered by name.
pub type Links = BTreeMap<String, PathBuf>;

/// The index of the directory entry at `rel`, relative to the package root.
pub fn dir_index(manifest: &Manifest, rel: &str) -> Option<usize> {
    (0..manifest.entries.len())
        .find(|&i| manifest.entries[i].kind == EntryKind::Dir && manifest.entry_path(i) == rel)
}

/// The directory the links go in, relative to the package root: the
/// shallowest one inside the package that the entrypoint's `$ORIGIN` run
/// path names, else the first library directory the package lists.
/// `runpath` holds the run path's directories as absolute paths.
///
/// The packer gives every object the entry that reaches the library
/// directory. A file in a subdirectory of it gets its own directory
/// first, and any directory its original run path named after, both
/// deeper than the library directory itself.
pub fn dir(
    manifest: &Manifest,
    pkg_root: &Path,
    runpath: &[PathBuf],
    lib_dirs: &[&str],
) -> Option<String> {
    let exists = |d: &&str| !d.is_empty() && dir_index(manifest, d).is_some();
    runpath
        .iter()
        .filter_map(|d| d.strip_prefix(pkg_root).ok()?.to_str())
        .filter(exists)
        .min_by_key(|d| Path::new(d).components().count())
        .or_else(|| lib_dirs.iter().copied().find(exists))
        .map(String::from)
}

/// The names the package itself has in `dir`.
fn carried<'m>(manifest: &'m Manifest, dir: &str) -> HashSet<&'m str> {
    let Some(idx) = dir_index(manifest, dir) else {
        return HashSet::new();
    };
    manifest
        .entries
        .iter()
        .filter(|e| e.parent as usize == idx)
        .map(|e| manifest.get_string(e.name))
        .collect()
}

/// The links a launch needs in `dir`: every library in the farm and in the
/// sets that the package does not carry there, the farm first and the sets
/// in order, so the first source of a name keeps it.
///
/// A set is linked from every directory in it that holds a shared object,
/// subdirectories included. A set library loaded through a link takes the
/// link's directory as its `$ORIGIN`, so a dependency its run path finds
/// in a subdirectory of the set has to be reachable from here too.
pub fn wanted(manifest: &Manifest, dir: &str, farm: Option<&Path>, set_roots: &[PathBuf]) -> Links {
    let carried = carried(manifest, dir);
    let mut links = Links::new();
    let mut add = |name: &OsStr, target: PathBuf| {
        if let Some(name) = name.to_str()
            && !carried.contains(name)
            && !links.contains_key(name)
        {
            links.insert(name.to_string(), target);
        }
    };
    if let Some(farm) = farm
        && let Ok(entries) = std::fs::read_dir(farm)
    {
        for entry in entries.flatten() {
            if let Ok(target) = std::fs::read_link(entry.path()) {
                add(&entry.file_name(), target);
            }
        }
    }
    for dir in set_roots.iter().flat_map(|r| crate::sets::lib_dirs(r)) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut found: Vec<_> = entries
            .flatten()
            .filter(|e| e.file_name().as_bytes().windows(3).any(|w| w == b".so"))
            .filter(|e| e.path().is_file())
            .map(|e| e.file_name())
            .collect();
        found.sort();
        for name in found {
            let target = dir.join(&name);
            add(&name, target);
        }
    }
    links
}

/// The links a launch before this one left in `dir` of the tree at `root`:
/// symlinks the package does not carry.
fn present(manifest: &Manifest, root: &Path, dir: &str) -> Links {
    let carried = carried(manifest, dir);
    let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
        return Links::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_symlink()))
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            let target = std::fs::read_link(e.path()).ok()?;
            (!carried.contains(name.as_str())).then_some((name, target))
        })
        .collect()
}

/// Make `links` the links in `dir` of the tree at `root`, removing any a
/// previous launch left that this one does not need.
///
/// Each change is atomic on its own, so a launch sharing the tree never
/// finds a name half made. The cache tree is the shared one: launches on
/// the same host want the same links, and one made under another host
/// state replaces what differs.
pub fn place(manifest: &Manifest, root: &Path, dir: &str, links: &Links) {
    let base = root.join(dir);
    let present = present(manifest, root, dir);
    for name in present.keys().filter(|n| !links.contains_key(*n)) {
        if let Err(e) = std::fs::remove_file(base.join(name)) {
            eprintln!(
                "onelf-rt: cannot remove the stale link {}: {e}",
                base.join(name).display()
            );
        }
    }
    for (name, target) in links {
        if present.get(name) == Some(target) {
            continue;
        }
        let path = base.join(name);
        let scratch = base.join(format!(".{name}.{}.link", std::process::id()));
        let _ = std::fs::remove_file(&scratch);
        let made = std::os::unix::fs::symlink(target, &scratch)
            .and_then(|()| std::fs::rename(&scratch, &path));
        if let Err(e) = made {
            let _ = std::fs::remove_file(&scratch);
            eprintln!(
                "onelf-rt: cannot link {} to {}: {e}",
                path.display(),
                target.display()
            );
        }
    }
}

/// Whether a mount another launch made serves exactly `links` in `dir`,
/// and none in the package's other library directories.
pub fn served(
    manifest: &Manifest,
    root: &Path,
    dir: &str,
    links: &Links,
    lib_dirs: &[&str],
) -> bool {
    present(manifest, root, dir) == *links
        && lib_dirs
            .iter()
            .filter(|d| **d != dir)
            .all(|d| present(manifest, root, d).is_empty())
}

/// The directories an ELF's `$ORIGIN` run path names, resolved against
/// the file's own directory. Empty for a file that is not an ELF or names
/// no `$ORIGIN` in either `DT_RPATH` or `DT_RUNPATH`.
pub fn origin_runpath(target: &Path) -> Vec<PathBuf> {
    let Ok(data) = std::fs::read(target) else {
        return Vec::new();
    };
    let Ok(elf) = goblin::elf::Elf::parse(&data) else {
        return Vec::new();
    };
    let origin = target.parent().unwrap_or(Path::new("/"));
    elf.rpaths
        .iter()
        .chain(elf.runpaths.iter())
        .flat_map(|p| p.split(':'))
        .filter_map(|entry| {
            let rest = entry
                .strip_prefix("$ORIGIN")
                .or_else(|| entry.strip_prefix("${ORIGIN}"))?;
            Some(normalize(&origin.join(rest.trim_start_matches('/'))))
        })
        .collect()
}

/// `path` with `.` and `..` components folded away, without consulting
/// the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// The launching child's side of the hand-off to the FUSE server.
pub struct Sender {
    tx: OwnedFd,
    ack: OwnedFd,
}

/// The FUSE server's side of the hand-off.
pub struct Receiver {
    rx: OwnedFd,
    ack: OwnedFd,
    buf: Vec<u8>,
}

/// What the server has heard from the child so far.
pub enum Received {
    /// More is on its way.
    Pending,
    /// The child is done: its links, or nothing when it went away or sent
    /// something unreadable.
    Done(Option<Message>),
}

/// The links for one directory, as the server is sent them.
pub struct Message {
    /// The directory, relative to the package root.
    pub dir: String,
    /// Each link's name and target.
    pub links: Vec<(String, Vec<u8>)>,
}

/// A pipe pair for handing the links from the child that resolves them to
/// the server that serves them. Made before the fork; the child keeps the
/// [`Sender`] and the server the [`Receiver`]. Every descriptor is
/// close-on-exec, so the app inherits none of them.
pub fn channel() -> io::Result<(Sender, Receiver)> {
    use rustix::pipe::{PipeFlags, pipe_with};
    let (rx, tx) = pipe_with(PipeFlags::CLOEXEC)?;
    let (ack_rx, ack_tx) = pipe_with(PipeFlags::CLOEXEC)?;
    Ok((
        Sender { tx, ack: ack_rx },
        Receiver {
            rx,
            ack: ack_tx,
            buf: Vec::new(),
        },
    ))
}

impl Sender {
    /// Send `links` for `dir` and wait until the server serves them. A
    /// server that has gone away ends the wait as well.
    pub fn send(&self, dir: &str, links: &Links) {
        let mut body = Vec::new();
        body.extend_from_slice(dir.as_bytes());
        body.push(0);
        for (name, target) in links {
            body.extend_from_slice(name.as_bytes());
            body.push(0);
            body.extend_from_slice(target.as_os_str().as_bytes());
            body.push(0);
        }
        let mut message = (body.len() as u64).to_le_bytes().to_vec();
        message.extend_from_slice(&body);
        let mut rest = message.as_slice();
        while !rest.is_empty() {
            match rustix::io::write(&self.tx, rest) {
                Ok(n) => rest = &rest[n..],
                Err(rustix::io::Errno::INTR) => {}
                Err(_) => return,
            }
        }
        let mut byte = [0u8; 1];
        while let Err(rustix::io::Errno::INTR) = rustix::io::read(&self.ack, &mut byte) {}
    }
}

impl Receiver {
    pub fn fd(&self) -> &OwnedFd {
        &self.rx
    }

    /// Take what the pipe holds. Call when [`Receiver::fd`] polls readable.
    pub fn read(&mut self) -> Received {
        let mut chunk = [0u8; 64 * 1024];
        match rustix::io::read(&self.rx, &mut chunk) {
            Ok(0) => return Received::Done(None),
            Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
            Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => return Received::Pending,
            Err(_) => return Received::Done(None),
        }
        let Some(len) = self.buf.get(..8) else {
            return Received::Pending;
        };
        let len = u64::from_le_bytes(len.try_into().unwrap_or_default()) as usize;
        match self.buf.get(8..8usize.saturating_add(len)) {
            Some(body) => Received::Done(decode(body)),
            None => Received::Pending,
        }
    }

    /// Tell the child its links are served.
    pub fn ack(&self) {
        let _ = rustix::io::write(&self.ack, b"k");
    }
}

fn decode(body: &[u8]) -> Option<Message> {
    let mut fields = body.split(|&b| b == 0);
    let dir = std::str::from_utf8(fields.next()?).ok()?.to_string();
    let mut links = Vec::new();
    while let Some(name) = fields.next().filter(|f| !f.is_empty()) {
        let target = fields.next()?;
        links.push((std::str::from_utf8(name).ok()?.to_string(), target.to_vec()));
    }
    Some(Message { dir, links })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runpath_entry_is_folded_against_the_files_directory() {
        assert_eq!(
            normalize(Path::new("/mnt/pkg/bin/../lib")),
            PathBuf::from("/mnt/pkg/lib")
        );
        assert_eq!(
            normalize(Path::new("/mnt/pkg/opt/x/bin/../../../lib/./sub")),
            PathBuf::from("/mnt/pkg/lib/sub")
        );
    }

    #[test]
    fn links_survive_the_trip_to_the_server() {
        let (tx, mut rx) = channel().unwrap();
        let mut links = Links::new();
        links.insert("libGL.so.1".into(), PathBuf::from("/usr/lib/libGL.so.1"));
        links.insert(
            "libfixture.so.1".into(),
            PathBuf::from("/run/set/lib/libfixture.so.1"),
        );
        let sender = std::thread::spawn(move || tx.send("lib", &links));
        let got = loop {
            if let Received::Done(got) = rx.read() {
                break got;
            }
        };
        rx.ack();
        sender.join().unwrap();
        let got = got.expect("a message");
        assert_eq!(got.dir, "lib");
        assert_eq!(
            got.links,
            vec![
                ("libGL.so.1".to_string(), b"/usr/lib/libGL.so.1".to_vec()),
                (
                    "libfixture.so.1".to_string(),
                    b"/run/set/lib/libfixture.so.1".to_vec()
                ),
            ]
        );
    }
}

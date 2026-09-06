//! Dependency sets a package shares with others instead of bundling.
//!
//! A set is the closure of some sysroot packages, packed as a build and
//! pinned by hash in `.onelf/sets`. The bundle carries none of it, so a
//! set is not conditional the way the GL build is: every set the package
//! names has to be there, and a set that cannot be obtained is a failed
//! launch rather than a degraded one.
//!
//! Obtaining one is the GL build's path exactly, so two packages naming
//! the same set share one download.
//!
//! What they do with it depends on the mode. Mounted, the set is served
//! from its stored file by a detached FUSE server at a mountpoint named
//! after the build's hash, so nothing is copied to disk and every package
//! naming that set shares one mount. The server lives in the host mount
//! namespace, which is why sets are obtained before any execution mode
//! makes a namespace of its own: a mount made afterwards would be private
//! to one launch. Extracted, the set goes through the ordinary package
//! cache, which costs a full copy but needs no FUSE.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::platform::Pin;

/// Where the package records the sets it needs, relative to the package
/// root.
pub const SETS_FILE: &str = ".onelf/sets";

/// One obtained set: the extracted root and the lock that keeps the
/// extraction alive for this instance.
pub struct Obtained {
    pub root: PathBuf,
    pub _lock: fs::File,
}

/// The sets the record names, in file order. A malformed table is
/// skipped rather than guessed at; the launch then fails on the missing
/// libraries, which names the problem better than a parse error would.
pub fn parse(text: &str) -> Vec<Pin> {
    let mut out: Vec<Pin> = Vec::new();
    let mut name: Option<String> = None;
    let (mut url, mut blake3) = (None, None);
    let flush = |name: &mut Option<String>,
                 url: &mut Option<String>,
                 blake3: &mut Option<String>,
                 out: &mut Vec<Pin>| {
        if let (Some(label), Some(url), Some(blake3)) = (name.take(), url.take(), blake3.take())
            && let Some(pin) = crate::platform::checked_pin(label, url, blake3)
        {
            out.push(pin);
        }
    };
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix('[') {
            flush(&mut name, &mut url, &mut blake3, &mut out);
            name = rest.strip_suffix(']').map(|n| n.trim().to_string());
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        let Some(value) = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .map(|v| v.replace("\\\"", "\"").replace("\\\\", "\\"))
        else {
            continue;
        };
        match key.trim() {
            "url" => url = Some(value),
            "blake3" => blake3 = Some(value.to_ascii_lowercase()),
            _ => {}
        }
    }
    flush(&mut name, &mut url, &mut blake3, &mut out);
    out
}

/// The sets this launch obtained, held for the life of the process: the
/// locks in them are what keep a mount or an extraction from being
/// reclaimed while the application runs, and they are inheritable so the
/// claim survives the `exec`.
static OBTAINED: OnceLock<Vec<Obtained>> = OnceLock::new();

/// Obtain every set the record names, before any execution mode makes a
/// mount namespace of its own. A set that cannot be had ends the launch:
/// the bundle carries none of it, so there is nothing to degrade to.
pub fn init(record: Option<&[u8]>) {
    let text = match record.map(String::from_utf8_lossy) {
        Some(text) => text.into_owned(),
        None => return,
    };
    let mut out = Vec::new();
    for pin in parse(&text) {
        match obtain(&pin) {
            Ok(o) => out.push(o),
            Err(why) => {
                eprintln!("onelf-rt: dependency set {}: {why}", pin.label);
                std::process::exit(1);
            }
        }
    }
    let _ = OBTAINED.set(out);
}

/// The extracted or mounted root of every set this launch obtained.
pub fn roots() -> Vec<PathBuf> {
    OBTAINED
        .get()
        .map(|sets| sets.iter().map(|s| s.root.clone()).collect())
        .unwrap_or_default()
}

/// Whether the set should be served from its stored file rather than
/// copied out of it. `ONELF_SET_MODE` forces either way; mounting is the
/// default and falls back to extraction wherever FUSE cannot serve.
fn mount_wanted() -> bool {
    !matches!(std::env::var("ONELF_SET_MODE").as_deref(), Ok("extract"))
}

fn obtain(pin: &Pin) -> Result<Obtained, String> {
    let file = crate::platform::store_build(pin)?;
    if mount_wanted() {
        match mount(&file, &pin.blake3) {
            Ok(obtained) => return Ok(obtained),
            Err(why) => eprintln!(
                "onelf-rt: dependency set {}: cannot mount ({why}); extracting instead",
                pin.label
            ),
        }
    }
    let (root, lock) = crate::platform::extract_build(&file)?;
    Ok(Obtained { root, _lock: lock })
}

/// Serve the build at `file` from a mountpoint named after `hash`, or
/// join the mount another instance already serves there.
fn mount(file: &Path, hash: &str) -> Result<Obtained, String> {
    use rustix::runtime::{Fork, kernel_fork};

    let (mountpoint, lock) =
        crate::paths::create_set_mountpoint(hash).ok_or("no private runtime directory")?;

    let dir_name = mountpoint
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    let joined = |lock: fs::File| Obtained {
        root: mountpoint.clone(),
        _lock: lock,
    };

    // Another instance is already serving this set: its mount is in the
    // host namespace, so it is ours to use as well. The shared lock just
    // taken is what stops it being reclaimed under us.
    if served(&mountpoint) {
        return Ok(joined(lock));
    }
    if !crate::fuse::mount::fusermount3_available() {
        return Err("fusermount3 is not available".into());
    }

    // One launch mounts, the rest wait and join what it made.
    let _creating = crate::paths::lock_set_creation(&dir_name);
    if served(&mountpoint) {
        return Ok(joined(lock));
    }

    match unsafe { kernel_fork() } {
        Ok(Fork::Child(_)) => {
            // The inherited lock is the parent's claim, not the server's:
            // closing this descriptor leaves the parent's open file
            // description holding it, and lets the server ask whether
            // anyone still does.
            drop(lock);
            serve(file, &mountpoint);
        }
        Ok(Fork::ParentOf(child)) => {
            for _ in 0..400 {
                if served(&mountpoint) {
                    return Ok(joined(lock));
                }
                if matches!(
                    rustix::process::waitpid(Some(child), rustix::process::WaitOptions::NOHANG),
                    Ok(Some(_))
                ) {
                    return Err("the server exited before the mount appeared".into());
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err("the mount did not appear".into())
        }
        Err(e) => Err(format!("fork: {e}")),
    }
}

/// Whether something is already serving `mountpoint`: a mount is there
/// and answers a directory read, which a mount whose server died does
/// not.
fn served(mountpoint: &Path) -> bool {
    crate::paths::is_mountpoint(mountpoint) && mountpoint.read_dir().is_ok()
}

/// The detached server: mount, serve until nothing claims the mountpoint
/// any more, then take it down. Never returns.
fn serve(file: &Path, mountpoint: &Path) -> ! {
    // Out of the launcher's session and off its standard streams, so a
    // Ctrl-C aimed at the application does not take the set down and
    // `app | head` does not wait on this process.
    let _ = rustix::process::setsid();
    if let Ok(null) = rustix::fs::open(
        "/dev/null",
        rustix::fs::OFlags::RDWR,
        rustix::fs::Mode::empty(),
    ) {
        for target in 0..=2 {
            use std::os::fd::FromRawFd;
            let mut slot =
                std::mem::ManuallyDrop::new(unsafe { std::os::fd::OwnedFd::from_raw_fd(target) });
            let _ = rustix::io::dup2(&null, &mut slot);
        }
    }

    let served = (|| -> Result<(), String> {
        let fuse_fd = crate::fuse::mount::fuse_mount(mountpoint).map_err(|e| e.to_string())?;
        let mut pkg = crate::loader::load_from(file).map_err(|e| e.to_string())?;
        let lock_path = crate::paths::set_lock_path(
            mountpoint
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default(),
        );
        let mut state = crate::fuse::fs::FuseState::new(
            &pkg.manifest,
            &mut pkg.file,
            &pkg.footer,
            pkg.dict.as_deref(),
        );
        let mut buf = vec![0u8; 1024 * 1024 + 4096];
        state.serve_detached(&fuse_fd, &mut buf, || {
            lock_path.as_deref().is_none_or(unclaimed)
        });
        drop(fuse_fd);
        Ok(())
    })();
    // A server that never mounted has nothing to take down, and
    // unmounting what is not mounted is a no-op, so one path serves both.
    let _ = served;
    crate::fuse::mount::fuse_unmount(mountpoint);
    let _ = std::fs::remove_dir(mountpoint);
    std::process::exit(0);
}

/// Whether nobody holds the mountpoint's shared lock any more, which is
/// what says the last package using this set is gone. Tested on a fresh
/// descriptor each time, since a lock belongs to the open file
/// description that took it.
fn unclaimed(lock_path: &Path) -> bool {
    let Ok(lock) = fs::File::open(lock_path) else {
        return false;
    };
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok()
}

/// The library directories a set root carries, in search order.
pub fn lib_dirs(root: &Path) -> Vec<PathBuf> {
    onelf_format::resolve::GL_BUILD_LIB_DIRS
        .iter()
        .map(|d| root.join(d))
        .filter(|d| d.is_dir())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_yields_one_pin_per_table() {
        let hash = "ab".repeat(32);
        let text = format!(
            "[qt]\nurl = \"file:///a/qt.onelf\"\nblake3 = \"{hash}\"\npackages = [\"qt6-base\"]\n\n\
             [gtk]\nurl = \"https://e/gtk.onelf\"\nblake3 = \"{hash}\"\npackages = []\n"
        );
        let pins = parse(&text);
        assert_eq!(pins.len(), 2);
        assert_eq!(pins[0].label, "qt");
        assert_eq!(pins[0].url, "file:///a/qt.onelf");
        assert_eq!(pins[1].label, "gtk");
        assert_eq!(pins[1].blake3, hash);
    }

    #[test]
    fn a_table_missing_a_field_or_carrying_a_bad_one_is_skipped() {
        let hash = "cd".repeat(32);
        let text = format!(
            "[nourl]\nblake3 = \"{hash}\"\n\n\
             [badhash]\nurl = \"file:///a\"\nblake3 = \"abc\"\n\n\
             [badname/x]\nurl = \"file:///a\"\nblake3 = \"{hash}\"\n\n\
             [ok]\nurl = \"file:///a\"\nblake3 = \"{hash}\"\n"
        );
        let pins = parse(&text);
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].label, "ok");
    }
}

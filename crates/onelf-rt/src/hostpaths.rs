//! Bundle directories made to answer for the absolute paths an
//! application was built with.
//!
//! Some applications open a path compiled into them, `/usr/share/name`
//! most often, and offer no environment variable to redirect it. A
//! bundle carries that directory, and on a host that has the
//! application installed the compiled-in path quietly finds the host's
//! copy instead; on a host that does not, it finds nothing.
//!
//! The recipe names the pairs and the runtime mounts them in a private
//! mount namespace, so the change is visible to the application and its
//! children and to nothing else on the machine. Where a namespace
//! cannot be had the launch says so once and goes on: the application
//! then behaves as it did before, which is to say it may fail, and the
//! reason is on the terminal.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use rustix::mount::{
    MountFlags, MountPropagationFlags, UnmountFlags, mount, mount_bind, mount_change, unmount,
};

/// Where the packer records the pairs, one `ABSOLUTE<tab>RELATIVE` per line.
pub const PATHS_FILE: &str = ".onelf/paths";

/// The pairs a package records, as absolute target and bundle-relative
/// source. A line without a tab, or naming a relative target, is skipped:
/// the packer refuses to write one, so anything else is a damaged file
/// rather than an instruction.
pub fn parse(text: &str) -> Vec<(PathBuf, PathBuf)> {
    text.lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(abs, rel)| (PathBuf::from(abs.trim()), PathBuf::from(rel.trim())))
        .filter(|(abs, rel)| abs.is_absolute() && !rel.as_os_str().is_empty())
        .collect()
}

/// Make each recorded bundle directory answer for its absolute path.
///
/// Called before the entrypoint is executed and after the package is in
/// place. Returns whether anything was mounted, so the caller can say so
/// when it matters.
pub fn apply(pkg_root: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(pkg_root.join(PATHS_FILE)) else {
        return false;
    };
    let wanted = parse(&text);
    if wanted.is_empty() {
        return false;
    }
    // A package that runs sudo or pkexec asks to stay in the host's user
    // namespace, because a setuid bit does nothing inside one we made
    // ourselves. The same switch that keeps the FUSE mount out of a
    // namespace keeps these mounts out of one.
    if std::path::Path::new(pkg_root)
        .join(".onelf/needs-setuid")
        .exists()
        || std::env::var_os("ONELF_FUSE_NO_NAMESPACE").is_some_and(|v| v != "0" && !v.is_empty())
    {
        eprintln!(
            "onelf-rt: this package stays in the host's user namespace, so the \
             paths it was built with are the host's"
        );
        return false;
    }
    if let Err(e) = enter() {
        eprintln!(
            "onelf-rt: cannot make the package's own directories answer for \
             the paths it was built with ({e}); it will read the host's"
        );
        return false;
    }
    let mut mounted = 0;
    // Targets that do not exist, by the directory they are to appear in.
    let mut absent: BTreeMap<PathBuf, Vec<(PathBuf, PathBuf)>> = BTreeMap::new();
    for (target, rel) in &wanted {
        let source = pkg_root.join(rel);
        if !source.is_dir() {
            continue;
        }
        if target.is_dir() {
            match mount_bind(&source, target) {
                Ok(()) => mounted += 1,
                Err(e) => eprintln!(
                    "onelf-rt: {}: binding {}: {e}",
                    target.display(),
                    source.display()
                ),
            }
            continue;
        }
        let (Some(parent), Some(name)) = (target.parent(), target.file_name()) else {
            eprintln!(
                "onelf-rt: {}: no parent to place it under",
                target.display()
            );
            continue;
        };
        if !parent.is_dir() {
            eprintln!(
                "onelf-rt: {}: {} does not exist either",
                target.display(),
                parent.display()
            );
            continue;
        }
        absent
            .entry(parent.to_path_buf())
            .or_default()
            .push((PathBuf::from(name), source));
    }
    if !absent.is_empty() {
        match add_names(&absent) {
            Ok(added) => mounted += added,
            Err(e) => eprintln!("onelf-rt: {e}"),
        }
    }
    mounted > 0
}

/// A private mount namespace of our own, with mounts that do not
/// propagate back to the host.
fn enter() -> io::Result<()> {
    crate::fuse::mount::enter_namespace()?;
    // Without this the bind below travels back up to whatever the host
    // shares, which is the opposite of the intent. A downstream mount
    // still hears from the host, so a disk mounted after launch reaches
    // the app.
    mount_change(
        "/",
        MountPropagationFlags::REC | MountPropagationFlags::DOWNSTREAM,
    )
    .map_err(|e| io::Error::other(format!("making mounts one-way: {e}")))?;
    Ok(())
}

/// Make each name appear in its parent directory, pointing at the
/// bundle's directory, and return how many did.
///
/// There is nothing to bind onto, so each parent gets one overlay: a
/// staging directory holding a symlink per name, laid over the parent so
/// the names are added without hiding what the parent already holds.
/// One overlay per parent rather than per name, since the kernel refuses
/// to stack overlays deeper than two, and the root of a container often
/// is one already.
///
/// The staging lives on a tmpfs mounted for the purpose. An overlay keeps
/// its own reference to every layer, so once the overlays are in place
/// the tmpfs is detached and its mountpoint removed: nothing is left on
/// disk, and it goes away with the namespace.
fn add_names(absent: &BTreeMap<PathBuf, Vec<(PathBuf, PathBuf)>>) -> io::Result<usize> {
    // Under the 0700 per-uid base: a shared /tmp name keyed only on the
    // pid is predictable across users, and the symlinks become an overlay
    // lowerdir the kernel resolves at mount time. No `onelf-` prefix, so
    // the mountpoint sweep leaves it alone.
    let base = crate::paths::private_dir()
        .ok_or_else(|| io::Error::other("no private runtime directory for overlay staging"))?;
    let staging = base.join(format!(
        "paths-{}",
        rustix::process::getpid().as_raw_nonzero()
    ));
    std::fs::create_dir_all(&staging)?;
    let added = mount(
        "tmpfs",
        &staging,
        "tmpfs",
        MountFlags::NOSUID | MountFlags::NODEV,
        Some(c"size=1m,mode=0700"),
    )
    .map_err(|e| io::Error::other(format!("staging tmpfs: {e}")))
    .map(|()| lay_overlays(&staging, absent));
    let _ = unmount(&staging, UnmountFlags::DETACH);
    let _ = std::fs::remove_dir(&staging);
    added
}

/// The overlays of [`add_names`], staged under `staging`.
fn lay_overlays(staging: &Path, absent: &BTreeMap<PathBuf, Vec<(PathBuf, PathBuf)>>) -> usize {
    let mut added = 0;
    for (i, (parent, names)) in absent.iter().enumerate() {
        let layer = staging.join(i.to_string());
        let staged = std::fs::create_dir(&layer).and_then(|()| {
            names
                .iter()
                .try_for_each(|(name, source)| std::os::unix::fs::symlink(source, layer.join(name)))
        });
        let laid = staged.map_err(|e| e.to_string()).and_then(|()| {
            let options = format!("lowerdir={}:{}", layer.display(), parent.display());
            let options = std::ffi::CString::new(options)
                .map_err(|_| "a path with a nul byte in it".to_string())?;
            mount(
                "overlay",
                parent,
                "overlay",
                MountFlags::RDONLY,
                Some(options.as_c_str()),
            )
            .map_err(|e| e.to_string())
        });
        match laid {
            Ok(()) => added += names.len(),
            Err(e) => eprintln!(
                "onelf-rt: laying the bundle's directories over {}: {e}",
                parent.display()
            ),
        }
    }
    added
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_are_read_and_nonsense_is_skipped() {
        let got = parse(
            "/usr/share/galculator\tshare/galculator\n\
             no-tab-here\n\
             relative/target\tshare/x\n\
             /usr/share/empty\t\n\
             /opt/vendor\tshare/vendor\n",
        );
        assert_eq!(
            got,
            vec![
                (
                    PathBuf::from("/usr/share/galculator"),
                    PathBuf::from("share/galculator")
                ),
                (PathBuf::from("/opt/vendor"), PathBuf::from("share/vendor")),
            ]
        );
    }
}

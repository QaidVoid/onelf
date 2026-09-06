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

use std::io;
use std::path::{Path, PathBuf};

use rustix::mount::{MountFlags, MountPropagationFlags, mount, mount_bind, mount_change};

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
    for (target, rel) in &wanted {
        let source = pkg_root.join(rel);
        if !source.is_dir() {
            continue;
        }
        match place(&source, target) {
            Ok(()) => mounted += 1,
            Err(e) => eprintln!("onelf-rt: {}: {e}", target.display()),
        }
    }
    mounted > 0
}

/// A private mount namespace of our own, with mounts that do not
/// propagate back to the host.
fn enter() -> io::Result<()> {
    crate::fuse::mount::enter_namespace()?;
    // Without this the bind below travels back up to whatever the host
    // shares, which is the opposite of the intent.
    mount_change(
        "/",
        MountPropagationFlags::REC | MountPropagationFlags::PRIVATE,
    )
    .map_err(|e| io::Error::other(format!("making mounts private: {e}")))?;
    Ok(())
}

/// Bind `source` over `target`, or, where the target does not exist,
/// lay the bundle's directory over the target's parent so the name
/// appears there.
fn place(source: &Path, target: &Path) -> io::Result<()> {
    if target.is_dir() {
        return mount_bind(source, target)
            .map_err(|e| io::Error::other(format!("binding {}: {e}", source.display())));
    }
    let Some(parent) = target.parent() else {
        return Err(io::Error::other("no parent to place it under"));
    };
    if !parent.is_dir() {
        return Err(io::Error::other(format!(
            "{} does not exist either",
            parent.display()
        )));
    }
    // The target is absent, so there is nothing to bind onto. An overlay
    // of the bundle's directory over the parent adds the name without
    // hiding what the parent already holds.
    let Some(name) = target.file_name() else {
        return Err(io::Error::other("no name to add"));
    };
    let staging = staging_dir(name, source)?;
    let options = format!("lowerdir={}:{}", staging.display(), parent.display());
    let options = std::ffi::CString::new(options)
        .map_err(|_| io::Error::other("a path with a nul byte in it"))?;
    mount(
        "overlay",
        parent,
        "overlay",
        MountFlags::RDONLY,
        Some(options.as_c_str()),
    )
    .map_err(|e| {
        io::Error::other(format!(
            "laying {} over {}: {e}",
            source.display(),
            parent.display()
        ))
    })
}

/// A directory holding one symlink, named as the target is, pointing at
/// the bundle's copy. Used as the upper layer of the overlay, so only
/// that one name is added to the parent.
fn staging_dir(name: &std::ffi::OsStr, source: &Path) -> io::Result<PathBuf> {
    let base = std::env::temp_dir().join(format!(
        "onelf-paths-{}",
        rustix::process::getpid().as_raw_nonzero()
    ));
    let dir = base.join(name);
    std::fs::create_dir_all(&dir)?;
    let link = dir.join(name);
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(source, &link)?;
    Ok(dir)
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

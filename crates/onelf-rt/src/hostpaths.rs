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
//!
//! The record is read from the package file and checked against its
//! content hash, never from the tree the launch runs in, and decoded by
//! the shared bounded codec. A record that fails either is refused
//! whole: nothing of it is mounted, and the launch says why and goes on
//! as it would without a namespace, so a damaged record never binds a
//! directory nobody asked for.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use onelf_format::paths::{self, MAX_RECORD_BYTES, PATHS_FILE, PathPair};
use onelf_format::plugindirs::{self, PLUGIN_DIRS_FILE, PluginDir};
use rustix::mount::{
    MountFlags, MountPropagationFlags, UnmountFlags, mount, mount_bind, mount_change, unmount,
};

use crate::loader::{self, PackageData};

/// The pairs the package records, read through the verified package
/// path. A package without the record has no pairs. A record that is
/// over its size bound, fails its content hash or does not decode is an
/// error, never an empty list, so the caller can say why nothing is
/// overlaid.
pub fn read(pkg: &PackageData) -> io::Result<Vec<PathPair>> {
    let Some(bytes) = loader::read_bounded_record(pkg, PATHS_FILE, MAX_RECORD_BYTES)? else {
        return Ok(Vec::new());
    };
    paths::decode(&bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{PATHS_FILE}: {e}")))
}

/// The plugin directories the package carries, read through the
/// verified package path like [`read`]. A package without the record
/// carries none; a record that is over its size bound, fails its
/// content hash or does not decode is an error, so the caller can say
/// why the host's modules are not considered.
pub fn read_plugin_dirs(pkg: &PackageData) -> io::Result<Vec<PluginDir>> {
    let Some(bytes) =
        loader::read_bounded_record(pkg, PLUGIN_DIRS_FILE, plugindirs::MAX_RECORD_BYTES)?
    else {
        return Ok(Vec::new());
    };
    plugindirs::decode(&bytes).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{PLUGIN_DIRS_FILE}: {e}"),
        )
    })
}

/// Make each bundle directory in `wanted` answer for its absolute path.
///
/// Called before the entrypoint is executed and after the package is in
/// place at `pkg_root`. Returns whether anything was mounted, so the
/// caller can say so when it matters.
pub fn apply(pkg_root: &Path, wanted: &[PathPair]) -> bool {
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
    for pair in wanted {
        let target = Path::new(&pair.absolute);
        let source = pkg_root.join(&pair.relative);
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
    use std::fs::File;

    use crate::pkgmeta::tests::fixture;

    use super::*;

    /// A fixture package carrying `files`, loaded through the real loader.
    fn load(name: &str, files: &[(&str, Vec<u8>)]) -> PackageData {
        load_bytes(
            name,
            &fixture::build(b"onelf fixture prefix", files, &[]).bytes,
        )
    }

    fn load_bytes(name: &str, bytes: &[u8]) -> PackageData {
        let path =
            std::env::temp_dir().join(format!("onelf-hostpaths-{name}-{}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        let pkg = loader::load_file(File::open(&path).unwrap()).unwrap();
        let _ = std::fs::remove_file(&path);
        pkg
    }

    #[test]
    fn a_package_without_the_record_has_no_pairs() {
        let pkg = load("absent", &[("bin/app", b"elf".to_vec())]);
        assert_eq!(read(&pkg).unwrap(), Vec::new());
    }

    #[test]
    fn a_package_without_the_plugin_dirs_record_carries_none() {
        let pkg = load("pd-absent", &[("bin/app", b"elf".to_vec())]);
        assert_eq!(read_plugin_dirs(&pkg).unwrap(), Vec::new());
    }

    #[test]
    fn the_plugin_dirs_record_is_read_from_the_package() {
        let record = b"lib/alsa-lib\talsa-lib\t/usr/lib/alsa-lib\nlib/xorg/modules\t";
        let pkg = load("pd-valid", &[(PLUGIN_DIRS_FILE, record.to_vec())]);
        let dirs = read_plugin_dirs(&pkg).unwrap();
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0].relative, "lib/alsa-lib");
        assert_eq!(dirs[0].lib_relative, "alsa-lib");
        assert_eq!(dirs[0].absolutes, vec!["/usr/lib/alsa-lib"]);
        assert_eq!(dirs[1].relative, "lib/xorg/modules");
        assert_eq!(dirs[1].lib_relative, "");
        assert!(dirs[1].absolutes.is_empty());
    }

    #[test]
    fn a_present_invalid_plugin_dirs_record_is_an_error_naming_the_directory() {
        let record = b"lib/alsa-lib\talsa-lib\n../outside\tx";
        let pkg = load("pd-escapes", &[(PLUGIN_DIRS_FILE, record.to_vec())]);
        let err = read_plugin_dirs(&pkg).unwrap_err().to_string();
        assert!(err.starts_with(PLUGIN_DIRS_FILE), "{err}");
        assert!(err.contains("\"../outside\""), "{err}");
    }

    #[test]
    fn the_record_is_read_from_the_package() {
        let record = b"/usr/share/galculator\tshare/galculator\n/opt/vendor\tshare/vendor";
        let pkg = load("valid", &[(PATHS_FILE, record.to_vec())]);
        let pairs = read(&pkg).unwrap();
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[1].absolute, "/opt/vendor");
        assert_eq!(pairs[1].relative, "share/vendor");
    }

    #[test]
    fn a_present_invalid_record_is_an_error_naming_the_pair() {
        let record = b"/usr/share/x\tshare/x\n/usr/share/y\t../outside";
        let pkg = load("escapes", &[(PATHS_FILE, record.to_vec())]);
        let err = read(&pkg).unwrap_err().to_string();
        assert!(err.starts_with(PATHS_FILE), "{err}");
        assert!(err.contains("\"/usr/share/y\" = \"../outside\""), "{err}");
    }

    #[test]
    fn a_tampered_record_is_refused_by_its_content_hash() {
        let built = fixture::build(
            b"onelf fixture prefix",
            &[(PATHS_FILE, b"/usr/share/x\tshare/x".to_vec())],
            &[],
        );
        let mut bytes = built.bytes;
        let (start, len) = built.spans[0];
        bytes[start + len - 1] = b'y';
        let pkg = load_bytes("tampered", &bytes);
        let err = read(&pkg).unwrap_err().to_string();
        assert!(
            err.starts_with(PATHS_FILE) && err.contains("hash mismatch"),
            "{err}"
        );
    }

    #[test]
    fn a_record_over_the_size_bound_is_refused_before_reading() {
        let mut pkg = load("oversized", &[(PATHS_FILE, b"/a\tb".to_vec())]);
        let idx = loader::find_file_entry(&pkg, PATHS_FILE).unwrap();
        pkg.manifest.entries[idx].blocks[0].original_size = MAX_RECORD_BYTES as u64 + 1;
        let err = read(&pkg).unwrap_err().to_string();
        assert!(
            err.starts_with(&format!("{PATHS_FILE}: declares")) && err.contains("record limit"),
            "{err}"
        );
    }
}

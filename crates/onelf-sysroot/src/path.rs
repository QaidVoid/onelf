//! Paths read inside a sysroot as if it were the root filesystem.
//!
//! A rootfs is full of absolute symlinks, `lib64 -> /usr/lib` and the
//! like, and they name the sysroot's own files. The packer's kernel
//! follows them to the packer's files instead, so every path read out of
//! a sysroot is resolved here first: an absolute target starts again at
//! the sysroot root, and `..` stops there.

use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};

/// More links than this on one path is taken for a loop, as the kernel
/// does.
const MAX_LINKS: usize = 40;

/// `rel` inside `root` with every symlink followed in sysroot terms,
/// returned as `root` joined with a path that holds no symlink. A
/// component that does not exist ends the resolution, and the rest is
/// appended as written, so the result exists exactly when `rel` does in
/// the sysroot.
pub fn in_root(root: &Path, rel: &Path) -> io::Result<PathBuf> {
    resolve(root, rel, true)
}

/// As [`in_root`], with the last component left as it is: a symlink there
/// is the link itself rather than what it names.
pub fn entry_in_root(root: &Path, rel: &Path) -> io::Result<PathBuf> {
    resolve(root, rel, false)
}

fn resolve(root: &Path, rel: &Path, follow_last: bool) -> io::Result<PathBuf> {
    let mut pending: Vec<OsString> = Vec::new();
    queue(&mut pending, rel);
    let mut out = PathBuf::new();
    let mut links = 0;
    while let Some(part) = pending.pop() {
        if part == ".." {
            out.pop();
            continue;
        }
        out.push(&part);
        if pending.is_empty() && !follow_last {
            break;
        }
        let Ok(target) = std::fs::read_link(root.join(&out)) else {
            continue;
        };
        links += 1;
        if links > MAX_LINKS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: too many levels of symbolic links", rel.display()),
            ));
        }
        out.pop();
        if target.is_absolute() {
            out.clear();
        }
        queue(&mut pending, &target);
    }
    Ok(root.join(out))
}

/// Push the components of `path` so the first is popped first.
fn queue(pending: &mut Vec<OsString>, path: &Path) {
    for c in path.components().rev() {
        match c {
            Component::Normal(name) => pending.push(name.to_os_string()),
            Component::ParentDir => pending.push("..".into()),
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::fixture::temp_root;
    use std::os::unix::fs::symlink;

    #[test]
    fn links_resolve_inside_the_root_and_never_leave_it() {
        let root = temp_root("inroot");
        std::fs::create_dir_all(root.join("usr/lib")).unwrap();
        std::fs::write(root.join("usr/lib/libfoo.so.1.2"), b"elf").unwrap();
        symlink("/usr/lib/libfoo.so.1.2", root.join("usr/lib/libfoo.so.1")).unwrap();
        symlink("/usr/lib", root.join("lib64")).unwrap();
        symlink("/home", root.join("usr/share")).unwrap();
        symlink("../../../../../..", root.join("usr/lib/up")).unwrap();
        symlink("loop", root.join("loop")).unwrap();

        assert_eq!(
            in_root(&root, Path::new("usr/lib/libfoo.so.1")).unwrap(),
            root.join("usr/lib/libfoo.so.1.2")
        );
        assert_eq!(
            in_root(&root, Path::new("lib64/libfoo.so.1")).unwrap(),
            root.join("usr/lib/libfoo.so.1.2")
        );
        assert_eq!(
            entry_in_root(&root, Path::new("lib64/libfoo.so.1")).unwrap(),
            root.join("usr/lib/libfoo.so.1")
        );
        assert_eq!(
            in_root(&root, Path::new("usr/share/me/.ssh/id_ed25519")).unwrap(),
            root.join("home/me/.ssh/id_ed25519")
        );
        assert_eq!(
            in_root(&root, Path::new("/../../etc/passwd")).unwrap(),
            root.join("etc/passwd")
        );
        assert_eq!(
            in_root(&root, Path::new("usr/lib/up/etc/passwd")).unwrap(),
            root.join("etc/passwd")
        );
        assert!(in_root(&root, Path::new("loop")).is_err());
    }
}

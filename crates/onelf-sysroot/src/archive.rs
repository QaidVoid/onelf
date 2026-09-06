//! Materializing a sysroot from a rootfs archive.
//!
//! No privileges are needed: ownership is not restored, and setuid and
//! setgid bits are dropped, since the bundle never needs either. Such a
//! file also gains owner read, because a mode like `---s--x---` leaves
//! the packer unable to read the sysroot it just wrote. Every
//! entry path is checked before anything is written, so an archive cannot
//! reach outside the directory it is unpacked into.
//!
//! Directory permissions are applied last. A rootfs holds directories a
//! distribution made read-only, and giving one its archived mode as soon
//! as it appears locks out every entry underneath it that has not been
//! written yet.

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

/// Unpack the `.tar` or `.tar.zst` archive at `archive` into `into`,
/// which is created if needed.
pub fn materialize(archive: &Path, into: &Path) -> io::Result<()> {
    let mut file = BufReader::new(File::open(archive)?);
    let mut magic = [0u8; 4];
    let n = file.read(&mut magic)?;
    let reader: Box<dyn Read> = if n == 4 && magic == ZSTD_MAGIC {
        let file = File::open(archive)?;
        Box::new(zstd::stream::read::Decoder::new(file)?)
    } else {
        Box::new(BufReader::new(File::open(archive)?))
    };

    std::fs::create_dir_all(into)?;
    let mut deferred: Vec<(PathBuf, u32)> = Vec::new();
    let mut tar = tar::Archive::new(reader);
    tar.set_preserve_permissions(true);
    tar.set_preserve_ownerships(false);
    tar.set_unpack_xattrs(false);
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if !is_contained(&path) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: entry escapes the target directory", path.display()),
            ));
        }
        let placed = entry
            .unpack_in(into)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {}", path.display(), chain(&e))))?;
        if !placed {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: entry could not be placed", path.display()),
            ));
        }
        let unpacked = into.join(&path);
        let Ok(md) = std::fs::symlink_metadata(&unpacked) else {
            continue;
        };
        let mode = md.permissions().mode();
        if md.file_type().is_dir() && mode & 0o300 != 0o300 {
            std::fs::set_permissions(&unpacked, std::fs::Permissions::from_mode(mode | 0o300))?;
            deferred.push((unpacked, mode & 0o7777));
        } else if md.file_type().is_file() && mode & 0o6000 != 0 {
            let stripped = (mode & 0o777) | 0o400;
            std::fs::set_permissions(&unpacked, std::fs::Permissions::from_mode(stripped))?;
        }
    }
    // Deepest first, so a read-only parent is closed after its children.
    deferred.sort_by(|a, b| b.0.cmp(&a.0));
    for (dir, mode) in deferred {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

/// An error and everything under it, joined. The tar crate reports the
/// path it could not place and keeps the reason it could not as the
/// source, which is the half worth reading.
fn chain(e: &dyn std::error::Error) -> String {
    let mut parts = vec![e.to_string()];
    let mut source = e.source();
    while let Some(inner) = source {
        parts.push(inner.to_string());
        source = inner.source();
    }
    parts.join(": ")
}

/// A relative path with no `..` and no root.
fn is_contained(path: &Path) -> bool {
    path.components().all(|c| match c {
        Component::Normal(_) | Component::CurDir => true,
        Component::ParentDir | Component::RootDir | Component::Prefix(_) => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::fixture::temp_root;

    fn archive_with(entries: &[(&str, &[u8], u32)], symlinks: &[(&str, &str)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, data, mode) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(*mode);
            header.set_uid(0);
            header.set_gid(0);
            if path.contains("..") {
                // The builder refuses such a name, which is exactly why the
                // reader has to: written straight into the header instead.
                let name = &mut header.as_old_mut().name;
                name[..path.len()].copy_from_slice(path.as_bytes());
                header.set_cksum();
                builder.append(&header, *data).unwrap();
            } else {
                header.set_cksum();
                builder.append_data(&mut header, path, *data).unwrap();
            }
        }
        for (path, target) in symlinks {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_mode(0o777);
            header.set_cksum();
            builder.append_link(&mut header, path, target).unwrap();
        }
        builder.into_inner().unwrap()
    }

    /// D-Bus ships its launch helper `---s--x---`. Dropping the setuid
    /// bit alone would leave a file the packer cannot read out of the
    /// sysroot it just wrote, so owner read comes with it.
    #[test]
    fn a_setuid_only_binary_is_left_readable() {
        let root = temp_root("tarsuid");
        let bytes = archive_with(
            &[("usr/lib/dbus-daemon-launch-helper", b"elf", 0o4110)],
            &[],
        );
        let archive = root.join("root.tar");
        std::fs::write(&archive, &bytes).unwrap();
        let into = root.join("sysroot");
        materialize(&archive, &into).unwrap();

        let helper = into.join("usr/lib/dbus-daemon-launch-helper");
        assert_eq!(std::fs::read(&helper).unwrap(), b"elf");
        let mode = std::fs::metadata(&helper).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o510, "setuid dropped, owner read added");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A distribution ships directories nothing may write to, and their
    /// contents come after them in the archive. Applying the mode as
    /// soon as the directory appears makes the rest of it unwritable, so
    /// the mode waits until the end.
    #[test]
    fn a_read_only_directory_still_receives_its_contents() {
        let root = temp_root("tarro");
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(0);
        header.set_mode(0o555);
        header.set_cksum();
        builder
            .append_data(&mut header, "etc/certs/", &[][..])
            .unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_size(3);
        header.set_mode(0o444);
        header.set_cksum();
        builder
            .append_data(&mut header, "etc/certs/ca.pem", &b"pem"[..])
            .unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        header.set_cksum();
        builder
            .append_link(&mut header, "etc/certs/0a1b2c3d.0", "ca.pem")
            .unwrap();
        let archive = root.join("root.tar");
        std::fs::write(&archive, builder.into_inner().unwrap()).unwrap();

        let into = root.join("sysroot");
        materialize(&archive, &into).unwrap();

        assert_eq!(
            std::fs::read(into.join("etc/certs/ca.pem")).unwrap(),
            b"pem"
        );
        assert!(into.join("etc/certs/0a1b2c3d.0").is_symlink());
        let mode = std::fs::metadata(into.join("etc/certs"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o555, "the archived mode is restored");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unpacks_files_modes_and_symlinks_without_ownership() {
        let root = temp_root("tar");
        let bytes = archive_with(
            &[
                ("usr/bin/app", b"#!/bin/sh\n", 0o4755),
                ("usr/lib/libfoo.so.1", b"elf", 0o644),
            ],
            &[("usr/lib/libfoo.so", "libfoo.so.1")],
        );
        let archive = root.join("root.tar");
        std::fs::write(&archive, &bytes).unwrap();
        let into = root.join("sysroot");
        materialize(&archive, &into).unwrap();
        assert_eq!(
            std::fs::read(into.join("usr/bin/app")).unwrap(),
            b"#!/bin/sh\n"
        );
        let mode = std::fs::metadata(into.join("usr/bin/app"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o755, "setuid is dropped, the rest is kept");
        assert_eq!(
            std::fs::read_link(into.join("usr/lib/libfoo.so")).unwrap(),
            Path::new("libfoo.so.1")
        );

        let zst = root.join("root.tar.zst");
        std::fs::write(&zst, zstd::encode_all(&bytes[..], 3).unwrap()).unwrap();
        let into = root.join("sysroot2");
        materialize(&zst, &into).unwrap();
        assert!(into.join("usr/lib/libfoo.so.1").is_file());
    }

    #[test]
    fn a_traversing_entry_is_refused_before_anything_is_written() {
        let root = temp_root("tarescape");
        let bytes = archive_with(&[("../escape", b"x", 0o644)], &[]);
        let archive = root.join("bad.tar");
        std::fs::write(&archive, &bytes).unwrap();
        let into = root.join("sysroot");
        let err = materialize(&archive, &into).unwrap_err();
        assert!(err.to_string().contains("escape"), "{err}");
        assert!(!root.join("escape").exists());
    }
}

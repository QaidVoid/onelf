//! Materializing a sysroot from a rootfs archive.
//!
//! No privileges are needed: ownership is not restored, and setuid and
//! setgid bits are dropped, since the bundle never needs either. Every
//! entry path is checked before anything is written, so an archive cannot
//! reach outside the directory it is unpacked into.

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path};

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
        if !entry.unpack_in(into)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: entry could not be placed", path.display()),
            ));
        }
        let unpacked = into.join(&path);
        if let Ok(md) = std::fs::symlink_metadata(&unpacked)
            && md.file_type().is_file()
            && md.permissions().mode() & 0o6000 != 0
        {
            std::fs::set_permissions(
                &unpacked,
                std::fs::Permissions::from_mode(md.permissions().mode() & 0o777),
            )?;
        }
    }
    Ok(())
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

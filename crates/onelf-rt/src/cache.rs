//! Cache-based extraction with content-addressable storage.
//!
//! Extracts package contents to `~/.cache/onelf/pkg/{package_id}/` using a CAS
//! (content-addressable store) for file deduplication. Files are stored by their
//! BLAKE3 hash and hardlinked into the package directory.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use onelf_format::EntryKind;

use crate::loader::{self, PackageData};

fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("onelf")
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Extract all package contents directly into `target_dir` without CAS.
/// Used for ephemeral tmpfs-backed extraction inside a private namespace,
/// where dedup has no value and the whole tree is thrown away on exit.
pub fn extract_direct(pkg: &mut PackageData, target_dir: &Path) -> io::Result<()> {
    let manifest = &pkg.manifest;

    // Dirs first
    for (i, entry) in manifest.entries.iter().enumerate() {
        if entry.kind == EntryKind::Dir {
            let path = manifest.entry_path(i);
            if !path.is_empty() {
                fs::create_dir_all(target_dir.join(&path))?;
            }
        }
    }

    // Files
    for (i, entry) in manifest.entries.iter().enumerate() {
        if entry.kind != EntryKind::File {
            continue;
        }
        let rel_path = manifest.entry_path(i);
        let out_path = target_dir.join(&rel_path);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let data = loader::read_payload_blocks(
            &mut pkg.file,
            pkg.footer.payload_offset,
            &entry.blocks,
            pkg.dict.as_deref(),
        )?;

        let mut f = fs::File::create(&out_path)?;
        f.write_all(&data)?;
        f.set_permissions(fs::Permissions::from_mode(entry.mode))?;
    }

    // Symlinks
    for (i, entry) in manifest.entries.iter().enumerate() {
        if entry.kind != EntryKind::Symlink {
            continue;
        }
        let rel_path = manifest.entry_path(i);
        let link_path = target_dir.join(&rel_path);
        let target = manifest.get_string(entry.symlink_target);

        if let Some(parent) = link_path.parent() {
            fs::create_dir_all(parent)?;
        }
        if link_path.symlink_metadata().is_ok() {
            fs::remove_file(&link_path)?;
        }
        std::os::unix::fs::symlink(target, &link_path)?;
    }

    Ok(())
}

pub fn ensure_extracted(pkg: &mut PackageData) -> io::Result<PathBuf> {
    let base = cache_dir();
    let package_id = hex(&pkg.manifest.header.package_id);
    let pkg_dir = base.join("pkg").join(&package_id);
    let cas_dir = base.join("cas");
    let lock_dir = base.join("lock");
    let meta_dir = base.join("meta");

    // Fast path: already extracted
    if pkg_dir.exists() {
        touch_meta(&meta_dir, &package_id);
        return Ok(pkg_dir);
    }

    // Take lock
    fs::create_dir_all(&lock_dir)?;
    let lock_path = lock_dir.join(&package_id);
    let lock_file = fs::File::create(&lock_path)?;
    rustix::fs::flock(&lock_file, rustix::fs::FlockOperation::LockExclusive)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("flock: {e}")))?;

    // Double-check after acquiring lock
    if pkg_dir.exists() {
        touch_meta(&meta_dir, &package_id);
        return Ok(pkg_dir);
    }

    fs::create_dir_all(&cas_dir)?;
    fs::create_dir_all(&pkg_dir)?;

    // Extract files to CAS and build hardlink farm
    extract_to_cas(pkg, &cas_dir, &pkg_dir)?;

    // Bundle-libs patches PT_INTERP to a relative path like
    // `lib/ld-linux-x86-64.so.2`. That resolves against the process
    // CWD at kernel exec time, which is fine for the entrypoint we
    // launch (the runtime chdirs to pkg_dir first) but breaks for
    // bundled ELFs that get fork+exec'd from some other CWD -
    // postgres spawning helpers from PGDATA, podman spawning
    // fuse-overlayfs from a container storage path, etc.
    //
    // In cache mode we know the absolute path the files live at, so
    // rewrite every bundled ELF's PT_INTERP to that absolute path.
    // The rewriter detaches hardlinks before writing so the CAS
    // entry stays pristine and other packages that share the same
    // hash are unaffected.
    rewrite_interps_absolute(&pkg_dir);

    // Record metadata
    touch_meta(&meta_dir, &package_id);

    // Lock released when lock_file goes out of scope
    Ok(pkg_dir)
}

/// Walk `pkg_dir` and rewrite every relative PT_INTERP to an
/// absolute path rooted at `pkg_dir`. Best-effort: failures on
/// individual files are logged to stderr but don't abort extraction.
fn rewrite_interps_absolute(pkg_dir: &Path) {
    walk_files(pkg_dir, &mut |path| {
        match crate::interp::rewrite_interp_absolute(path, pkg_dir) {
            Ok(_) => {}
            Err(e) => eprintln!(
                "onelf-rt: warning: failed to rewrite PT_INTERP of {}: {e}",
                path.display()
            ),
        }
    });
}

fn walk_files(dir: &Path, f: &mut dyn FnMut(&Path)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            walk_files(&path, f);
        } else if meta.is_file() {
            f(&path);
        }
    }
}

fn extract_to_cas(pkg: &mut PackageData, cas_dir: &Path, pkg_dir: &Path) -> io::Result<()> {
    let manifest = &pkg.manifest;

    // First pass: create directories
    for (i, entry) in manifest.entries.iter().enumerate() {
        if entry.kind == EntryKind::Dir {
            let path = manifest.entry_path(i);
            if !path.is_empty() {
                fs::create_dir_all(pkg_dir.join(&path))?;
            }
        }
    }

    // Second pass: extract files to CAS and create hardlinks
    for (i, entry) in manifest.entries.iter().enumerate() {
        if entry.kind != EntryKind::File {
            continue;
        }

        let hash_hex = hex(&entry.content_hash);
        let shard = &hash_hex[..2];
        let cas_shard_dir = cas_dir.join(shard);
        let cas_path = cas_shard_dir.join(&hash_hex);

        // Check if already in CAS (dedup)
        if !cas_path.exists() {
            fs::create_dir_all(&cas_shard_dir)?;

            let data = loader::read_payload_blocks(
                &mut pkg.file,
                pkg.footer.payload_offset,
                &entry.blocks,
                pkg.dict.as_deref(),
            )?;

            // Atomic write: temp file then rename
            let tmp_path = cas_shard_dir.join(format!(".{hash_hex}.tmp"));
            {
                let mut f = fs::File::create(&tmp_path)?;
                f.write_all(&data)?;
                f.set_permissions(fs::Permissions::from_mode(entry.mode))?;
            }
            fs::rename(&tmp_path, &cas_path)?;
        }

        // Hardlink into pkg dir (avoids readlink issues with symlinks)
        let rel_path = manifest.entry_path(i);
        let link_path = pkg_dir.join(&rel_path);

        if let Some(parent) = link_path.parent() {
            fs::create_dir_all(parent)?;
        }

        if link_path.symlink_metadata().is_ok() {
            fs::remove_file(&link_path)?;
        }
        fs::hard_link(&cas_path, &link_path)?;
    }

    // Third pass: create symlinks
    for (i, entry) in manifest.entries.iter().enumerate() {
        if entry.kind != EntryKind::Symlink {
            continue;
        }

        let rel_path = manifest.entry_path(i);
        let link_path = pkg_dir.join(&rel_path);
        let target = manifest.get_string(entry.symlink_target);

        if let Some(parent) = link_path.parent() {
            fs::create_dir_all(parent)?;
        }
        if link_path.symlink_metadata().is_ok() {
            fs::remove_file(&link_path)?;
        }
        std::os::unix::fs::symlink(target, &link_path)?;
    }

    Ok(())
}

fn touch_meta(meta_dir: &Path, package_id: &str) {
    let _ = fs::create_dir_all(meta_dir);
    let meta_path = meta_dir.join(package_id);
    let _ = fs::File::create(&meta_path);
}

pub fn remove_package(base: &Path, package_id: &str) {
    let _ = fs::remove_dir_all(base.join("pkg").join(package_id));
    let _ = fs::remove_file(base.join("meta").join(package_id));
    let _ = fs::remove_file(base.join("lock").join(package_id));
}

pub fn auto_gc(base: &Path, max_age_secs: u64, current_pkg_id: &str) {
    let meta_dir = base.join("meta");
    let entries = match fs::read_dir(&meta_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let now = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs(),
        Err(_) => return,
    };

    let mut removed = 0u32;
    for entry in entries.flatten() {
        if removed >= 5 {
            break;
        }

        let name = entry.file_name();
        let id = name.to_string_lossy();
        if id == current_pkg_id {
            continue;
        }

        let mtime = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            Err(_) => continue,
        };

        if now.saturating_sub(mtime) > max_age_secs {
            remove_package(base, &id);
            removed += 1;
        }
    }
}

pub fn base_dir() -> PathBuf {
    cache_dir()
}

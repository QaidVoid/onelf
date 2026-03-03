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

    // Record metadata
    touch_meta(&meta_dir, &package_id);

    // Lock released when lock_file goes out of scope
    Ok(pkg_dir)
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

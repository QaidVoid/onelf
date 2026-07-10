//! Detection and runtime workarounds for self-extracting binaries.
//!
//! Self-extracting binaries (e.g. pre-1.3.12 Bun-compiled apps) read
//! `/proc/self/exe` to find their embedded payload. When onelf invokes
//! the bundled dynamic linker explicitly, `/proc/self/exe` points at
//! the linker, not the binary, so payload detection fails.
//!
//! `prctl(PR_SET_MM_EXE_FILE)` could fix this in principle but the
//! kernel guards it behind `CAP_SYS_RESOURCE` (privileged). Instead we
//! make the kernel handle PT_INTERP itself, then kernel-exec the binary
//! directly so `/proc/self/exe` resolves to it.
//!
//! Two strategies (tried in order):
//!
//! 1. **Bind-mount** (FUSE/tmpfs modes only — they have a private mount
//!    namespace): bind-mount the bundled linker over the binary's
//!    existing PT_INTERP path (e.g. `/lib64/ld-linux-x86-64.so.2`).
//!    Invisible outside the namespace, no host pollution.
//!
//! 2. **`/tmp` symlink + PT_INTERP patch** (fallback for cache mode):
//!    create a deterministic-name symlink under `/tmp` pointing at the
//!    bundled linker, then in-place patch the binary's PT_INTERP to
//!    that path. The new path is shorter than the original so the
//!    file size doesn't change and any self-extract trailer at the end
//!    is preserved.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Detect the Bun self-extract trailer at the end of an ELF file.
///
/// Pre-1.3.12 Bun-compiled binaries end with the magic
/// `\n---- Bun! ----\n`, optionally followed by an 8-byte length word.
pub fn has_self_extract_trailer(path: &Path) -> bool {
    const TRAILER: &[u8] = b"\n---- Bun! ----\n";
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let Ok(meta) = file.metadata() else {
        return false;
    };
    if meta.len() < 24 {
        return false;
    }

    let mut buf = [0u8; 24];
    if file.seek(SeekFrom::End(-24)).is_err() {
        return false;
    }
    if file.read_exact(&mut buf).is_err() {
        return false;
    }

    // Bare trailer at end (modern format): bytes 8..24
    if &buf[8..24] == TRAILER {
        return true;
    }
    // Trailer followed by 8-byte length (pre-1.3.12 format): bytes 0..16
    if &buf[0..16] == TRAILER {
        return true;
    }
    false
}

/// Read the binary's `PT_INTERP` value (the absolute interpreter path
/// the kernel will look up at exec time).
fn read_pt_interp(binary: &Path) -> io::Result<String> {
    let mut data = vec![0u8; 8192];
    let mut file = fs::File::open(binary)?;
    let n = file.read(&mut data)?;
    data.truncate(n);

    if data.len() < 64 || data[0..4] != *b"\x7fELF" {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not an ELF"));
    }
    if data[4] != 2 {
        return Err(io::Error::new(io::ErrorKind::Unsupported, "not 64-bit ELF"));
    }

    let e_phoff = u64::from_le_bytes(data[32..40].try_into().unwrap()) as usize;
    let e_phentsize = u16::from_le_bytes(data[54..56].try_into().unwrap()) as usize;
    let e_phnum = u16::from_le_bytes(data[56..58].try_into().unwrap()) as usize;

    for i in 0..e_phnum {
        let off = e_phoff + i * e_phentsize;
        if off + e_phentsize > data.len() {
            break;
        }
        let p_type = u32::from_le_bytes(data[off..off + 4].try_into().unwrap());
        if p_type != 3 {
            continue;
        }
        let p_offset = u64::from_le_bytes(data[off + 8..off + 16].try_into().unwrap()) as usize;
        let p_filesz = u64::from_le_bytes(data[off + 32..off + 40].try_into().unwrap()) as usize;

        // PT_INTERP may point past the first 8KB; re-read if needed.
        if p_offset + p_filesz > data.len() {
            file.seek(SeekFrom::Start(p_offset as u64))?;
            let mut buf = vec![0u8; p_filesz];
            file.read_exact(&mut buf)?;
            let s = std::str::from_utf8(strip_nul(&buf))
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "PT_INTERP not UTF-8"))?;
            return Ok(s.to_string());
        }
        let s = std::str::from_utf8(strip_nul(&data[p_offset..p_offset + p_filesz]))
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "PT_INTERP not UTF-8"))?;
        return Ok(s.to_string());
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no PT_INTERP entry",
    ))
}

fn strip_nul(buf: &[u8]) -> &[u8] {
    match buf.iter().position(|&b| b == 0) {
        Some(p) => &buf[..p],
        None => buf,
    }
}

/// Bind-mount the bundled linker over the binary's PT_INTERP path
/// inside the current mount namespace. After this, kernel exec of the
/// binary will follow PT_INTERP and find the bundled linker.
///
/// This requires:
///   - the caller is in a private mount namespace (FUSE/tmpfs modes)
///   - the PT_INTERP path's parent directory exists on the host
///     (so we have a target to mount over). For NixOS, `/lib64` exists
///     as a stub directory; chromatic's PT_INTERP `/lib64/ld-linux-x86-64.so.2`
///     resolves to a stub link we can mount over.
///
/// Returns the PT_INTERP path on success (for diagnostics).
pub fn bind_mount_interp(binary: &Path, bundled_linker: &Path) -> io::Result<PathBuf> {
    let interp = read_pt_interp(binary)?;
    let interp_path = PathBuf::from(&interp);

    if !interp_path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("PT_INTERP is not absolute: {interp}"),
        ));
    }

    let parent = interp_path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("PT_INTERP has no parent: {interp}"),
        )
    })?;
    if !parent.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "PT_INTERP parent dir doesn't exist on host: {}",
                parent.display()
            ),
        ));
    }

    // Bind-mount source must be a regular file (or symlink to one).
    // The target must exist - on NixOS /lib64/ld-linux-x86-64.so.2 is a
    // stub symlink, on other distros it's the real linker. Either way,
    // we replace it with the bundled linker for this namespace only.
    rustix::mount::mount_bind(bundled_linker, &interp_path).map_err(|e| {
        io::Error::other(format!(
            "bind-mount {} -> {} failed: {e}",
            bundled_linker.display(),
            interp_path.display()
        ))
    })?;

    Ok(interp_path)
}

/// Fallback for environments without a private mount namespace (cache
/// mode): create a short `/tmp` symlink to the bundled linker, then
/// in-place patch the binary's PT_INTERP to that symlink.
///
/// The symlink path is deterministic from the linker's content hash so
/// multiple invocations share a single symlink. The patched PT_INTERP
/// must fit in the existing slot (`/tmp/onelf-ld-<8hex>.so` is 22 bytes
/// — comfortably under the typical 28-byte slot of `/lib64/ld-linux-...`).
///
/// On success, returns the symlink path that PT_INTERP now points at.
pub fn symlink_interp(binary: &Path, bundled_linker: &Path) -> io::Result<PathBuf> {
    let canonical_linker = bundled_linker.canonicalize()?;

    // Hash by the linker's *path* so our symlink target stays valid
    // across invocations from the same cache. Hashing by content would
    // be more correct but require reading the whole file.
    let hash = simple_hash(canonical_linker.to_string_lossy().as_bytes());
    // Place the symlink inside our 0700 uid-owned private dir so no other
    // user can pre-create or tamper with it. Security comes from the owned
    // directory, not from an unpredictable name.
    let base = crate::paths::private_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "onelf: no private runtime dir for interp symlink",
        )
    })?;
    let link_path = base.join(format!("onelf-ld-{hash:08x}.so"));

    // Create or refresh the symlink (idempotent).
    let needs_create = match fs::read_link(&link_path) {
        Ok(existing) => existing != canonical_linker,
        Err(_) => true,
    };
    if needs_create {
        // Atomic update: create at a temp path, then rename. Avoids a
        // window where the symlink doesn't exist for concurrent runs.
        let tmp = base.join(format!("onelf-ld-{hash:08x}.tmp"));
        let _ = fs::remove_file(&tmp);
        std::os::unix::fs::symlink(&canonical_linker, &tmp)?;
        fs::rename(&tmp, &link_path)?;
    }

    // In-place patch the binary's PT_INTERP to point at our /tmp symlink.
    let link_str = link_path.to_string_lossy();
    patch_pt_interp_in_place(binary, &link_str)?;

    Ok(link_path)
}

/// In-place rewrite the binary's PT_INTERP to `new_interp`. Fails if
/// the new path doesn't fit in the existing slot (file size must stay
/// the same so any self-extract trailer at the end is preserved).
fn patch_pt_interp_in_place(binary: &Path, new_interp: &str) -> io::Result<()> {
    let mut data = fs::read(binary)?;
    if data.len() < 64 || data[0..4] != *b"\x7fELF" {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not an ELF"));
    }
    if data[4] != 2 {
        return Err(io::Error::new(io::ErrorKind::Unsupported, "not 64-bit ELF"));
    }

    let e_phoff = u64::from_le_bytes(data[32..40].try_into().unwrap()) as usize;
    let e_phentsize = u16::from_le_bytes(data[54..56].try_into().unwrap()) as usize;
    let e_phnum = u16::from_le_bytes(data[56..58].try_into().unwrap()) as usize;

    let new_bytes = new_interp.as_bytes();

    for i in 0..e_phnum {
        let off = e_phoff + i * e_phentsize;
        if off + e_phentsize > data.len() {
            break;
        }
        let p_type = u32::from_le_bytes(data[off..off + 4].try_into().unwrap());
        if p_type != 3 {
            // PT_INTERP = 3
            continue;
        }
        let p_offset = u64::from_le_bytes(data[off + 8..off + 16].try_into().unwrap()) as usize;
        let p_filesz = u64::from_le_bytes(data[off + 32..off + 40].try_into().unwrap()) as usize;

        if new_bytes.len() + 1 > p_filesz {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "new PT_INTERP ({} bytes) doesn't fit in slot ({} bytes)",
                    new_bytes.len() + 1,
                    p_filesz
                ),
            ));
        }
        if p_offset + p_filesz > data.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "PT_INTERP slot extends past file",
            ));
        }

        // No-op if already patched to the same value.
        let current_end = (p_offset..p_offset + p_filesz)
            .find(|&i| data[i] == 0)
            .unwrap_or(p_offset + p_filesz);
        if &data[p_offset..current_end] == new_bytes {
            return Ok(());
        }

        data[p_offset..p_offset + new_bytes.len()].copy_from_slice(new_bytes);
        for j in (p_offset + new_bytes.len())..(p_offset + p_filesz) {
            data[j] = 0;
        }

        // Break any hardlink to CAS by writing through a temp + rename.
        let tmp = binary.with_extension("interp-patch");
        fs::write(&tmp, &data)?;
        let _ = fs::set_permissions(
            &tmp,
            <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        );
        fs::rename(&tmp, binary)?;
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no PT_INTERP entry",
    ))
}

/// FNV-1a 32-bit hash for naming.
fn simple_hash(data: &[u8]) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
}

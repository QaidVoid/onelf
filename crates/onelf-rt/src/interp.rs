//! ELF interpreter detection and bundled interpreter invocation.
//!
//! When a packed binary's ELF interpreter (PT_INTERP) doesn't exist on the
//! host system (e.g. running a glibc binary on musl), the runtime can fall
//! back to a bundled interpreter from the package's lib directories.
//!
//! Two execution modes:
//! 1. userland-execve: Maps interpreter directly, bypasses kernel loader (preferred)
//! 2. Command-based: Invokes interpreter via --argv0 (fallback for non-ELF entrypoints)

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Read the PT_INTERP (ELF interpreter path) from a binary file.
/// Only reads the first 8KB — enough for ELF headers and the interp string.
pub fn read_elf_interp(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 8192];
    let n = file.read(&mut buf).ok()?;
    buf.truncate(n);
    parse_elf_interp(&buf)
}

/// Rewrite a bundled ELF's PT_INTERP to an absolute path rooted at
/// `pkg_root`. Only runs when the current interp is *relative* (i.e.
/// the string does not start with `/`).
///
/// Why this exists: bundle-libs patches PT_INTERP to a relative path
/// like `lib/ld-linux-x86-64.so.2`, which the kernel resolves against
/// the process CWD at exec time. That works for the entrypoint the
/// runtime spawns (we chdir to the AppDir before execve), but it
/// breaks any time the entrypoint fork+execs another bundled ELF with
/// a different CWD. Postgres spawning helpers from `$PGDATA`, podman
/// spawning `fuse-overlayfs` from a container storage dir, etc.
///
/// This fix only applies in cache mode, where the AppDir sits at a
/// stable absolute path we can hard-code into every bundled binary.
/// Returns Ok(false) if there's nothing to do (non-ELF, no PT_INTERP,
/// already absolute).
pub fn rewrite_interp_absolute(path: &Path, pkg_root: &Path) -> std::io::Result<bool> {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(_) => return Ok(false),
    };
    if data.len() < 64 || data[0..4] != *b"\x7fELF" {
        return Ok(false);
    }
    let class = data[4];
    if class != 2 {
        // 32-bit ELFs are rare in modern bundles; skip to keep this
        // hand-rolled patcher small.
        return Ok(false);
    }
    let e_phoff = u64::from_le_bytes(data[32..40].try_into().unwrap()) as usize;
    let e_phentsize = u16::from_le_bytes(data[54..56].try_into().unwrap()) as usize;
    let e_phnum = u16::from_le_bytes(data[56..58].try_into().unwrap()) as usize;

    for i in 0..e_phnum {
        let phdr_off = e_phoff + i * e_phentsize;
        if phdr_off + e_phentsize > data.len() {
            return Ok(false);
        }
        let p_type = u32::from_le_bytes(data[phdr_off..phdr_off + 4].try_into().unwrap());
        if p_type != 3 {
            continue; // PT_INTERP = 3
        }
        let p_offset = u64::from_le_bytes(data[phdr_off + 8..phdr_off + 16].try_into().unwrap())
            as usize;
        let p_filesz = u64::from_le_bytes(data[phdr_off + 32..phdr_off + 40].try_into().unwrap())
            as usize;
        if p_offset + p_filesz > data.len() {
            return Ok(false);
        }

        let cur = &data[p_offset..p_offset + p_filesz];
        let cur = match cur.iter().position(|&b| b == 0) {
            Some(pos) => &cur[..pos],
            None => cur,
        };
        if cur.is_empty() || cur[0] == b'/' {
            return Ok(false);
        }

        let mut absolute = pkg_root.as_os_str().as_encoded_bytes().to_vec();
        if !absolute.ends_with(b"/") {
            absolute.push(b'/');
        }
        absolute.extend_from_slice(cur);
        // Do the write via a fresh file so the hardlink to CAS is
        // broken without changing the shared blob. Using
        // O_TRUNC|O_WRONLY on the original path would mutate the CAS
        // inode and ripple into every other package that shares the
        // same content hash.
        let mut modified = data.clone();
        let new_len = absolute.len() + 1; // NUL terminator
        if new_len <= p_filesz {
            // Fits in the existing slot.
            modified[p_offset..p_offset + absolute.len()].copy_from_slice(&absolute);
            for b in &mut modified[p_offset + absolute.len()..p_offset + p_filesz] {
                *b = 0;
            }
            // Shrink p_filesz / p_memsz so readelf and similar don't
            // see the trailing NULs as part of the path.
            modified[phdr_off + 32..phdr_off + 40]
                .copy_from_slice(&(new_len as u64).to_le_bytes());
            modified[phdr_off + 40..phdr_off + 48]
                .copy_from_slice(&(new_len as u64).to_le_bytes());
        } else {
            // Append the new string to the end of the file and
            // repoint the phdr. PT_INTERP is read via pread from
            // p_offset, it does not need to be inside a PT_LOAD.
            let new_offset = modified.len() as u64;
            modified.extend_from_slice(&absolute);
            modified.push(0);
            modified[phdr_off + 8..phdr_off + 16].copy_from_slice(&new_offset.to_le_bytes());
            modified[phdr_off + 32..phdr_off + 40]
                .copy_from_slice(&(new_len as u64).to_le_bytes());
            modified[phdr_off + 40..phdr_off + 48]
                .copy_from_slice(&(new_len as u64).to_le_bytes());
        }

        // Unlink the hardlink first so we write a fresh inode.
        let mode = std::fs::metadata(path)
            .map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()))
            .unwrap_or(0o755);
        let _ = std::fs::remove_file(path);
        std::fs::write(path, &modified)?;
        let _ = std::fs::set_permissions(
            path,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(mode),
        );
        return Ok(true);
    }
    Ok(false)
}

fn parse_elf_interp(data: &[u8]) -> Option<String> {
    if data.len() < 64 || data[0..4] != *b"\x7fELF" {
        return None;
    }

    let class = data[4];

    let (e_phoff, e_phentsize, e_phnum) = match class {
        2 => {
            let e_phoff = u64::from_le_bytes(data[32..40].try_into().ok()?) as usize;
            let e_phentsize = u16::from_le_bytes(data[54..56].try_into().ok()?) as usize;
            let e_phnum = u16::from_le_bytes(data[56..58].try_into().ok()?) as usize;
            (e_phoff, e_phentsize, e_phnum)
        }
        1 => {
            let e_phoff = u32::from_le_bytes(data[28..32].try_into().ok()?) as usize;
            let e_phentsize = u16::from_le_bytes(data[42..44].try_into().ok()?) as usize;
            let e_phnum = u16::from_le_bytes(data[44..46].try_into().ok()?) as usize;
            (e_phoff, e_phentsize, e_phnum)
        }
        _ => return None,
    };

    for i in 0..e_phnum {
        let off = e_phoff + i * e_phentsize;
        if off + e_phentsize > data.len() {
            break;
        }

        let p_type = u32::from_le_bytes(data[off..off + 4].try_into().ok()?);
        if p_type != 3 {
            continue;
        }

        let (p_offset, p_filesz) = match class {
            2 => {
                let p_offset =
                    u64::from_le_bytes(data[off + 8..off + 16].try_into().ok()?) as usize;
                let p_filesz =
                    u64::from_le_bytes(data[off + 32..off + 40].try_into().ok()?) as usize;
                (p_offset, p_filesz)
            }
            1 => {
                let p_offset = u32::from_le_bytes(data[off + 4..off + 8].try_into().ok()?) as usize;
                let p_filesz =
                    u32::from_le_bytes(data[off + 16..off + 20].try_into().ok()?) as usize;
                (p_offset, p_filesz)
            }
            _ => return None,
        };

        if p_offset + p_filesz > data.len() {
            return None;
        }

        let interp = &data[p_offset..p_offset + p_filesz];
        let interp = match interp.iter().position(|&b| b == 0) {
            Some(pos) => &interp[..pos],
            None => interp,
        };
        return std::str::from_utf8(interp).ok().map(String::from);
    }

    None
}

/// Check if we should use userland-execve for this target.
///
/// Returns the bundled interpreter path if:
/// - Target is a PIE ELF binary (ET_DYN)
/// - Bundled interpreter exists
/// - userland-execve is supported on this platform
///
/// Non-PIE ELFs (ET_EXEC) go through the command-based fallback instead
/// because userland-execve can't relocate them.
pub fn should_use_userland_exec(
    target: &Path,
    pkg_root: &Path,
    bundled_interp_rel: Option<&str>,
) -> Option<PathBuf> {
    if !crate::ulexec::is_supported() {
        return None;
    }

    let rel_path = bundled_interp_rel?;
    let interp = pkg_root.join(rel_path);

    if !interp.exists() {
        return None;
    }

    if read_elf_interp(target).is_none() {
        return None;
    }

    if !is_pie(target) {
        return None;
    }

    Some(interp)
}

/// Read the ELF e_type field and return true for ET_DYN (PIE / shared object).
fn is_pie(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 20];
    if f.read(&mut buf).unwrap_or(0) < 20 {
        return false;
    }
    if buf[0..4] != *b"\x7fELF" {
        return false;
    }
    // e_type is at offset 16 as u16 little-endian. ET_DYN = 3.
    let e_type = u16::from_le_bytes([buf[16], buf[17]]);
    e_type == 3
}

/// Execute an ELF binary using userland-execve with bundled interpreter.
///
/// This function never returns on success.
pub fn exec_userland(target: &Path, interpreter: &Path, argv0: &str, args: &[String]) -> ! {
    crate::ulexec::exec_with_interp(target, interpreter, argv0, args)
}

/// Search for the interpreter in the package's lib directories.
///
/// Match by the PT_INTERP's own basename (e.g. `ld-linux-x86-64.so.2`),
/// not by whatever the host's symlink points at. On NixOS the host's
/// `/lib64/ld-linux-x86-64.so.2` is a symlink to a `stub-ld-*` store
/// path, and resolving the symlink would make us look for a file named
/// `stub-ld-...` in the bundle, which of course doesn't exist; we'd
/// then fall back to the kernel-loaded stub and fail.
fn find_bundled_interp(interp: &str, pkg_root: &Path, lib_dirs: &[&str]) -> Option<PathBuf> {
    let interp_name = Path::new(interp).file_name()?.to_os_string();

    for dir in lib_dirs {
        let candidate = pkg_root.join(dir).join(&interp_name);
        if candidate.exists() {
            return Some(candidate);
        }
    }

    let candidate = pkg_root.join(&interp_name);
    if candidate.exists() {
        return Some(candidate);
    }

    None
}

/// Build a `Command` for executing the target binary.
///
/// Preferred path: the packed binary was run through `bundle-libs`,
/// which rewrites PT_INTERP to a path relative to the package root
/// (like `lib/ld-linux-x86-64.so.2`). We can then execve the target
/// directly with CWD=pkg_root, and the kernel loads the bundled ld.
/// That keeps `/proc/self/exe` on the target itself — Python, Electron
/// and Qt all rely on that.
///
/// Fallback path: PT_INTERP is absolute and unpatched (e.g. the user
/// packed without bundling). We invoke the bundled loader explicitly
/// with `--argv0`, which works but leaves /proc/self/exe on the ld.
///
/// The `force_cwd` returned value is Some(pkg_root) when the caller
/// must chdir before exec, and None otherwise.
pub fn build_exec_command(
    target: &Path,
    pkg_root: &Path,
    lib_dirs: &[&str],
    argv0: &str,
    args: &[String],
) -> (Command, Option<PathBuf>) {
    use std::os::unix::process::CommandExt;

    if let Some(interp) = read_elf_interp(target) {
        let interp_path = Path::new(&interp);
        if interp_path.is_relative() {
            let resolved = pkg_root.join(interp_path);
            if resolved.exists() {
                let mut cmd = Command::new(target);
                cmd.arg0(argv0).args(args);
                return (cmd, Some(pkg_root.to_path_buf()));
            }
        }
        if let Some(bundled) = find_bundled_interp(&interp, pkg_root, lib_dirs) {
            let lib_path = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
            let is_musl = interp_path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("ld-musl-"));

            let mut cmd = Command::new(&bundled);
            // glibc's ld-linux supports --inhibit-cache; musl's ld-musl rejects it.
            if !is_musl {
                cmd.arg("--inhibit-cache");
            }
            if !lib_path.is_empty() {
                cmd.arg("--library-path").arg(&lib_path);
            }
            cmd.arg("--argv0").arg(argv0).arg(target).args(args);
            return (cmd, None);
        }
        if !interp_path.exists() {
            eprintln!(
                "onelf-rt: warning: ELF interpreter '{}' not found on this system \
                 and no bundled equivalent in the AppDir",
                interp
            );
        }
    }

    // Non-ELF target (shell wrapper, python script, etc). If the bundle
    // has a patched dynamic loader in one of its lib dirs, any ELF the
    // wrapper execs will have a relative PT_INTERP that only resolves
    // when CWD is pkg_root. Force it so child execves don't ENOENT.
    let force_cwd = if has_bundled_loader(pkg_root, lib_dirs) {
        Some(pkg_root.to_path_buf())
    } else {
        None
    };
    let mut cmd = Command::new(target);
    cmd.arg0(argv0).args(args);
    (cmd, force_cwd)
}

/// True if any of `lib_dirs` under `pkg_root` holds a file whose name
/// looks like a dynamic loader (ld-linux-*, ld-musl-*). A bundled loader
/// is the signal that bundle-libs patched PT_INTERP of the bundled ELFs
/// to a relative path, so the runtime must control CWD at exec time.
fn has_bundled_loader(pkg_root: &Path, lib_dirs: &[&str]) -> bool {
    for dir in lib_dirs {
        let d = pkg_root.join(dir);
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with("ld-linux") || name.starts_with("ld-musl-") {
                return true;
            }
        }
    }
    false
}

/// Parse the bundled interpreter relative path from `.onelf/interp` metadata.
pub fn parse_bundled_interp_rel(interp_data: &[u8]) -> Option<&str> {
    std::str::from_utf8(interp_data).ok()?.lines().next()
}

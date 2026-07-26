//! ELF interpreter detection and bundled interpreter invocation.
//!
//! When a packed binary's ELF interpreter (PT_INTERP) doesn't exist on the
//! host system (e.g. running a glibc binary on musl), the runtime can fall
//! back to a bundled interpreter from the package's lib directories.
//!
//! Two execution modes:
//! 1. userland-exec: Maps interpreter directly, bypasses kernel loader (preferred)
//! 2. Command-based: Invokes interpreter via --argv0 (fallback for non-ELF entrypoints)

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Read the PT_INTERP (ELF interpreter path) from a binary file.
///
/// The initial 8 KB read covers the ELF header and program-header table for
/// the slot lookup; the interp string itself is then read at its own file
/// offset, which may be far into the file (e.g. after `patchelf` relocates
/// `.interp`), so it is not limited to the first 8 KB.
pub fn read_elf_interp(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = vec![0u8; 8192];
    let n = file.read(&mut head).ok()?;
    head.truncate(n);
    let (p_offset, p_filesz) = pt_interp_slot(&head)?;

    // Fast path: the interp string is already within the header window.
    if let Some(s) = head.get(p_offset..p_offset.checked_add(p_filesz)?) {
        return interp_from_bytes(s);
    }
    // Otherwise read it at its file offset.
    file.seek(SeekFrom::Start(p_offset as u64)).ok()?;
    let mut buf = vec![0u8; p_filesz];
    file.read_exact(&mut buf).ok()?;
    interp_from_bytes(&buf)
}

/// Trim at the first NUL and decode as UTF-8.
fn interp_from_bytes(bytes: &[u8]) -> Option<String> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).ok().map(String::from)
}

/// Locate the PT_INTERP program header in `data` (which must contain the ELF
/// header and program-header table) and return the interpreter string's
/// `(file offset, size-in-file)`. Little-endian 32- and 64-bit ELF; every
/// read is bounds-checked against `data`. The interp bytes themselves may lie
/// beyond `data` (the caller reads them, possibly re-reading the file).
pub(crate) fn pt_interp_slot(data: &[u8]) -> Option<(usize, usize)> {
    if data.len() < 64 || data[0..4] != *b"\x7fELF" {
        return None;
    }
    let class = data[4];
    let (e_phoff, e_phentsize, e_phnum) = match class {
        2 => (
            u64::from_le_bytes(data.get(32..40)?.try_into().ok()?) as usize,
            u16::from_le_bytes(data.get(54..56)?.try_into().ok()?) as usize,
            u16::from_le_bytes(data.get(56..58)?.try_into().ok()?) as usize,
        ),
        1 => (
            u32::from_le_bytes(data.get(28..32)?.try_into().ok()?) as usize,
            u16::from_le_bytes(data.get(42..44)?.try_into().ok()?) as usize,
            u16::from_le_bytes(data.get(44..46)?.try_into().ok()?) as usize,
        ),
        _ => return None,
    };
    // Each program-header entry must be large enough to hold the fields we
    // read below (p_offset / p_filesz); reject malformed tables up front.
    let min_phentsize = if class == 2 { 56 } else { 32 };
    if e_phentsize < min_phentsize {
        return None;
    }
    for i in 0..e_phnum {
        let off = e_phoff.checked_add(i.checked_mul(e_phentsize)?)?;
        let end = off.checked_add(e_phentsize)?;
        if end > data.len() {
            break;
        }
        let p_type = u32::from_le_bytes(data.get(off..off + 4)?.try_into().ok()?);
        if p_type != 3 {
            continue;
        }
        return match class {
            2 => Some((
                u64::from_le_bytes(data.get(off + 8..off + 16)?.try_into().ok()?) as usize,
                u64::from_le_bytes(data.get(off + 32..off + 40)?.try_into().ok()?) as usize,
            )),
            1 => Some((
                u32::from_le_bytes(data.get(off + 4..off + 8)?.try_into().ok()?) as usize,
                u32::from_le_bytes(data.get(off + 16..off + 20)?.try_into().ok()?) as usize,
            )),
            _ => None,
        };
    }
    None
}

/// In-buffer variant of [`read_elf_interp`], used by tests that build a
/// synthetic ELF in memory.
#[cfg(test)]
fn parse_elf_interp(data: &[u8]) -> Option<String> {
    let (p_offset, p_filesz) = pt_interp_slot(data)?;
    let interp = data.get(p_offset..p_offset.checked_add(p_filesz)?)?;
    interp_from_bytes(interp)
}

/// Check if we should use userland-exec for this target.
///
/// Returns the bundled interpreter path if:
/// - Target is a PIE ELF binary (ET_DYN)
/// - Bundled interpreter exists
/// - userland-exec is supported on this platform
///
/// Non-PIE ELFs (ET_EXEC) go through the command-based fallback instead
/// because userland-exec can't relocate them.
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

    // Self-extracting binaries (e.g. pre-1.3.12 Bun) need /proc/self/exe
    // to resolve to the binary itself. userland-exec doesn't update
    // /proc/self/exe, so we route these through the kernel-exec path
    // (see build_exec_command's self-extract handling).
    if crate::selfextract::has_self_extract_trailer(target) {
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

/// Execute an ELF binary using userland-exec with bundled interpreter.
///
/// `lib_path` is passed to the linker via `--library-path` instead of via
/// the inherited `LD_LIBRARY_PATH` env var, so bundled libs aren't visible
/// to child processes the app spawns.
///
/// This function never returns on success.
pub fn exec_userland(
    target: &Path,
    interpreter: &Path,
    lib_path: &str,
    argv0: &str,
    args: &[String],
) -> ! {
    crate::ulexec::exec_with_interp(target, interpreter, lib_path, argv0, args)
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
/// With the AT_EXECFN bootstrap, bundled ELFs resolve their own
/// interpreter relative to the binary's location. No CWD control needed.
///
/// Fallback: if PT_INTERP is absolute and unpatched (packed without
/// bundling), invoke the bundled loader explicitly with `--argv0`.
pub fn build_exec_command(
    target: &Path,
    pkg_root: &Path,
    lib_dirs: &[&str],
    lib_path: &str,
    private_ns: bool,
    argv0: &str,
    args: &[String],
) -> Command {
    use std::os::unix::process::CommandExt;

    if let Some(interp) = read_elf_interp(target) {
        let interp_path = Path::new(&interp);
        if let Some(bundled) = find_bundled_interp(&interp, pkg_root, lib_dirs) {
            // Self-extracting binaries (pre-1.3.12 Bun, etc.) read
            // /proc/self/exe to find their embedded payload. The
            // explicit linker invocation below sets /proc/self/exe to
            // the linker, which breaks payload detection. We need a
            // direct kernel-exec of the binary so /proc/self/exe
            // resolves to it.
            //
            // To make the kernel resolve PT_INTERP (typically
            // /lib64/ld-linux-x86-64.so.2) to our bundled linker:
            //   - In a private mount namespace (FUSE/tmpfs): bind-mount
            //     the bundled linker over PT_INTERP. Invisible outside.
            //   - Otherwise (cache mode): create a /tmp symlink and
            //     in-place patch PT_INTERP to point at it.
            if crate::selfextract::has_self_extract_trailer(target) {
                let prepped = if private_ns {
                    crate::selfextract::bind_mount_interp(target, &bundled).map(|_| ())
                } else {
                    crate::selfextract::symlink_interp(target, &bundled).map(|_| ())
                };
                match prepped {
                    Ok(()) => {
                        let mut cmd = Command::new(target);
                        cmd.arg0(argv0).args(args);
                        if !lib_path.is_empty() {
                            cmd.env("LD_LIBRARY_PATH", lib_path);
                        }
                        return cmd;
                    }
                    Err(e) => {
                        eprintln!(
                            "onelf-rt: warning: self-extract prep failed for {}: {e}; \
                             falling back to explicit linker invocation",
                            target.display()
                        );
                    }
                }
            }

            let is_musl = interp_path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("ld-musl-"));

            let mut cmd = Command::new(&bundled);
            if !is_musl {
                cmd.arg("--inhibit-cache");
            }
            if !lib_path.is_empty() {
                cmd.arg("--library-path").arg(lib_path);
            }
            cmd.arg("--argv0").arg(argv0).arg(target).args(args);
            return cmd;
        }
        if !interp_path.exists() {
            eprintln!(
                "onelf-rt: warning: ELF interpreter '{}' not found on this system \
                 and no bundled equivalent in the AppDir",
                interp
            );
        }
    }

    let mut cmd = Command::new(target);
    cmd.arg0(argv0).args(args);
    cmd
}

/// Parse the bundled interpreter relative path from `.onelf/interp` metadata.
pub fn parse_bundled_interp_rel(interp_data: &[u8]) -> Option<&str> {
    std::str::from_utf8(interp_data).ok()?.lines().next()
}

#[cfg(test)]
mod interp_tests {
    use super::*;

    fn elf64_with_interp(interp: &str) -> Vec<u8> {
        let phoff = 64usize;
        let phentsize = 56usize;
        let interp_off = phoff + phentsize;
        let mut v = vec![0u8; interp_off + interp.len() + 1];
        v[0..4].copy_from_slice(b"\x7fELF");
        v[4] = 2; // ELFCLASS64
        v[32..40].copy_from_slice(&(phoff as u64).to_le_bytes());
        v[54..56].copy_from_slice(&(phentsize as u16).to_le_bytes());
        v[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum
        v[phoff..phoff + 4].copy_from_slice(&3u32.to_le_bytes()); // PT_INTERP
        v[phoff + 8..phoff + 16].copy_from_slice(&(interp_off as u64).to_le_bytes());
        v[phoff + 32..phoff + 40].copy_from_slice(&((interp.len() + 1) as u64).to_le_bytes());
        v[interp_off..interp_off + interp.len()].copy_from_slice(interp.as_bytes());
        v
    }

    #[test]
    fn slot_and_parse_agree() {
        let name = "/lib64/ld-linux-x86-64.so.2";
        let elf = elf64_with_interp(name);
        let (off, sz) = pt_interp_slot(&elf).expect("slot");
        assert_eq!(off, 120);
        assert_eq!(sz, name.len() + 1);
        assert_eq!(parse_elf_interp(&elf).as_deref(), Some(name));
    }

    #[test]
    fn malformed_returns_none_without_panic() {
        assert!(pt_interp_slot(b"not an elf").is_none());
        assert!(pt_interp_slot(&[]).is_none());
        // ELF magic but an out-of-bounds program-header table.
        let mut short = vec![0u8; 64];
        short[0..4].copy_from_slice(b"\x7fELF");
        short[4] = 2;
        short[56..58].copy_from_slice(&9999u16.to_le_bytes());
        short[32..40].copy_from_slice(&(1u64 << 40).to_le_bytes());
        assert!(pt_interp_slot(&short).is_none());
    }
}

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
/// - Target is an ELF binary
/// - Bundled interpreter exists
/// - userland-execve is supported on this platform
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

    Some(interp)
}

/// Execute an ELF binary using userland-execve with bundled interpreter.
///
/// This function never returns on success.
pub fn exec_userland(target: &Path, interpreter: &Path, argv0: &str, args: &[String]) -> ! {
    crate::ulexec::exec_with_interp(target, interpreter, argv0, args)
}

/// Search for the interpreter in the package's lib directories.
fn find_bundled_interp(interp: &str, pkg_root: &Path, lib_dirs: &[&str]) -> Option<PathBuf> {
    let interp_path = Path::new(interp);

    let interp_name = std::fs::read_link(interp_path)
        .ok()
        .and_then(|target| target.file_name().map(|n| n.to_os_string()))
        .or_else(|| interp_path.file_name().map(|n| n.to_os_string()))?;

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
/// This is a fallback for cases where userland-execve cannot be used:
/// - Non-ELF files (shell scripts, etc.)
/// - System interpreter exists
/// - Platform doesn't support userland-execve
///
/// For ELF binaries with missing interpreter, invokes the bundled interpreter
/// directly with --argv0.
pub fn build_exec_command(
    target: &Path,
    pkg_root: &Path,
    lib_dirs: &[&str],
    argv0: &str,
    args: &[String],
) -> Command {
    use std::os::unix::process::CommandExt;

    if let Some(interp) = read_elf_interp(target) {
        if !Path::new(&interp).exists() {
            if let Some(bundled) = find_bundled_interp(&interp, pkg_root, lib_dirs) {
                let lib_path = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();

                let mut cmd = Command::new(&bundled);
                cmd.arg("--inhibit-cache");
                if !lib_path.is_empty() {
                    cmd.arg("--library-path").arg(&lib_path);
                }
                cmd.arg("--argv0").arg(argv0).arg(target).args(args);
                return cmd;
            }
            eprintln!(
                "onelf-rt: warning: ELF interpreter '{}' not found on this system",
                interp
            );
            eprintln!(
                "onelf-rt: hint: bundle the interpreter with: onelf bundle-libs --exclude ''"
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

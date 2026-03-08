//! ELF interpreter detection, bundled interpreter fallback, and symlink setup.
//!
//! When a packed binary's ELF interpreter (PT_INTERP) doesn't exist on the
//! host system (e.g. running a glibc binary on musl), the runtime can fall
//! back to a bundled interpreter from the package's lib directories.
//!
//! For packages packed with PT_INTERP patching, all ELF binaries point to a
//! short symlink (`/tmp/.oi<hash8>`) that the runtime creates at startup,
//! targeting either the system interpreter or the bundled one.

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

    let class = data[4]; // 1 = 32-bit, 2 = 64-bit

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
            continue; // Not PT_INTERP
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
        // Truncate at first NUL — patched binaries pad the shorter new path with NULs
        let interp = match interp.iter().position(|&b| b == 0) {
            Some(pos) => &interp[..pos],
            None => interp,
        };
        return std::str::from_utf8(interp).ok().map(String::from);
    }

    None // Statically linked
}

/// Search for the interpreter in the package's lib directories.
fn find_bundled_interp(interp: &str, pkg_root: &Path, lib_dirs: &[&str]) -> Option<PathBuf> {
    let interp_path = Path::new(interp);

    // If interp is a symlink (e.g. patched /tmp/.oiXXXX), resolve it to get
    // the real interpreter filename. read_link works even on dead symlinks.
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

    // Also check the package root directly
    let candidate = pkg_root.join(&interp_name);
    if candidate.exists() {
        return Some(candidate);
    }

    None
}

/// Set up the interpreter symlink for cross-libc portability.
///
/// Reads `.onelf/interp` metadata (injected at pack time) and creates a symlink
/// at the specified path pointing to either the system interpreter or the
/// bundled one. This makes all ELF binaries in the package work regardless
/// of how they're invoked (directly or via shell scripts).
pub fn setup_interp_symlink(interp_data: &[u8], pkg_root: &Path) {
    let text = match std::str::from_utf8(interp_data) {
        Ok(t) => t,
        Err(_) => return,
    };

    let mut lines = text.lines();
    let original = match lines.next() {
        Some(s) if !s.is_empty() => s,
        _ => return,
    };
    let symlink_str = match lines.next() {
        Some(s) if !s.is_empty() => s,
        _ => return,
    };
    let bundled_rel = match lines.next() {
        Some(s) if !s.is_empty() => s,
        _ => return,
    };

    let symlink_path = Path::new(symlink_str);

    // Prefer system interpreter if available, otherwise use bundled
    let target = if Path::new(original).exists() {
        PathBuf::from(original)
    } else {
        pkg_root.join(bundled_rel)
    };

    // Idempotent: check if symlink already points to the right target
    if let Ok(existing) = std::fs::read_link(symlink_path) {
        if existing == target {
            return;
        }
        let _ = std::fs::remove_file(symlink_path);
    }

    let _ = std::os::unix::fs::symlink(&target, symlink_path);
}

/// Build a `Command` for executing the target binary.
///
/// If the binary's ELF interpreter doesn't exist on this system but a bundled
/// copy is found in the package's lib dirs, returns a command that invokes the
/// bundled interpreter with `--argv0` to preserve the original program name.
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
            // Interpreter not found (e.g. cross-libc: glibc binary on musl host).
            // Fall back to invoking the bundled interpreter directly with
            // --inhibit-cache and --library-path to avoid host library contamination.
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
        // Interpreter exists (via system or interp symlink) — use normal exec
        // so /proc/self/exe points to the actual binary (needed by Python, etc.)
    }

    let mut cmd = Command::new(target);
    cmd.arg0(argv0).args(args);
    cmd
}

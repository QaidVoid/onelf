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
    // An interpreter path is a short string (bounded by PATH_MAX); reject
    // implausibly large or out-of-file entries before allocating the fallback
    // buffer, so a corrupt ELF can't drive a huge allocation.
    const MAX_INTERP: usize = 4096;

    let mut file = std::fs::File::open(path).ok()?;
    let file_len = file.metadata().ok()?.len();
    let mut head = vec![0u8; 8192];
    let n = file.read(&mut head).ok()?;
    head.truncate(n);
    let (p_offset, p_filesz) = onelf_format::elf::pt_interp_slot(&head)?;

    if p_filesz == 0 || p_filesz > MAX_INTERP {
        return None;
    }
    let end = p_offset.checked_add(p_filesz)?;
    if end as u64 > file_len {
        return None;
    }

    // Fast path: the interp string is already within the header window.
    if let Some(s) = head.get(p_offset..end) {
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

    read_elf_interp(target)?;

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

    // Nothing above claimed this binary, so no linker invocation of ours
    // carries --library-path for it. A bootstrap-injected binary reaches the
    // bundled libraries through the RPATH the packer wrote, so only the host
    // paths are named here: those are what the bundled linker cannot find on
    // its own, and unlike our own libc they do no harm to anything the app
    // goes on to run.
    let mut cmd = Command::new(target);
    cmd.arg0(argv0).args(args);

    let host_only: Vec<&str> = lib_path
        .split(':')
        .filter(|dir| !dir.is_empty() && !Path::new(dir).starts_with(pkg_root))
        .collect();
    if !host_only.is_empty() {
        cmd.env("LD_LIBRARY_PATH", host_only.join(":"));
    }

    cmd
}

/// Parse the bundled interpreter relative path from `.onelf/interp` metadata.
pub fn parse_bundled_interp_rel(interp_data: &[u8]) -> Option<&str> {
    std::str::from_utf8(interp_data).ok()?.lines().next()
}

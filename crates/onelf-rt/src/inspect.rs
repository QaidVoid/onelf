//! Flags that let a user look inside a package without the `onelf` tool:
//! `--onelf-help`, `--onelf-version`, `--onelf-extract`, `--onelf-mount`
//! and `--onelf-doctor`.

use std::ffi::OsString;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};

use crate::loader::PackageData;

const HELP: &str = "\
Runtime flags, taken by the runtime wherever they appear. Every other
argument goes to the app.

  --onelf-help                 List these flags
  --onelf-version              Print the runtime and package versions
  --onelf-doctor               Show what a launch would do, without running the app
  --onelf-extract [DIR] [PATTERN...]
                               Extract the package, or the paths matching a
                               PATTERN, to DIR (default: onelf_extracted)
  --onelf-mount                Mount the package and print the mount point,
                               until interrupted
  --onelf-icon                 Print the bundled icon
  --onelf-desktop              Print the bundled .desktop file
  --onelf-integrate            Install the .desktop file and icons
  --onelf-unintegrate          Remove what --onelf-integrate installed
  --onelf-portable             Create all portable directories
  --onelf-portable-home        Create <binary>.home
  --onelf-portable-config      Create <binary>.config
  --onelf-portable-share       Create <binary>.share
  --onelf-portable-cache       Create <binary>.cache
";

const UPDATE_HELP: &str = "\
  --onelf-check-update         Check for an update
  --onelf-update               Download and install an update
";

fn has(args: &[OsString], flag: &str) -> bool {
    args.iter().any(|a| a.as_os_str() == flag)
}

/// Handle `--onelf-help`, which needs no package. Returns `true` if it
/// was given.
pub fn handle_help(args: &[OsString]) -> bool {
    if !has(args, "--onelf-help") {
        return false;
    }
    print!("{HELP}");
    if cfg!(feature = "update") {
        print!("{UPDATE_HELP}");
    }
    true
}

/// Handle the flags of this module that need the package. Returns the
/// exit code when one was given.
pub fn handle_flags(args: &[OsString], pkg: &mut PackageData, ep_idx: usize) -> Option<i32> {
    if has(args, "--onelf-version") {
        version(pkg);
        Some(0)
    } else if let Some(at) = args.iter().position(|a| a == "--onelf-extract") {
        Some(extract(pkg, &args[at + 1..]))
    } else if has(args, "--onelf-mount") {
        Some(mount(pkg))
    } else if has(args, "--onelf-doctor") {
        crate::doctor::run(pkg, ep_idx);
        Some(0)
    } else {
        None
    }
}

fn version(pkg: &mut PackageData) {
    println!("onelf-rt {}", env!("CARGO_PKG_VERSION"));
    let info = crate::read_package_file(pkg, ".onelf/package-info.toml")
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let version = info_value(&info, "version");
    let name = match pkg.manifest.name() {
        "" => info_value(&info, "name").unwrap_or_default(),
        name => name.to_string(),
    };
    match version {
        Some(version) => println!("{name} {version}"),
        None => println!("{name}"),
    }
}

/// A `key = "value"` line of `.onelf/package-info.toml`, which the packer
/// writes as TOML basic strings.
fn info_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (k, v) = line.split_once('=')?;
        if k.trim() != key {
            return None;
        }
        let v = v.trim().strip_prefix('"')?.strip_suffix('"')?;
        Some(v.replace("\\\"", "\"").replace("\\\\", "\\"))
    })
}

/// Whether `pattern` names `path` or a directory above it. `*` matches any
/// run of characters, `/` included.
fn matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim_start_matches("./").trim_end_matches('/');
    let p = pattern.as_bytes();
    let s = path.as_bytes();
    glob(p, s) || (0..s.len()).any(|i| s[i] == b'/' && glob(p, &s[..i]))
}

fn glob(p: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some((pi, si));
            pi += 1;
        } else if pi < p.len() && p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else if let Some((sp, ss)) = star {
            pi = sp + 1;
            si = ss + 1;
            star = Some((sp, ss + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == b'*')
}

fn extract(pkg: &mut PackageData, args: &[OsString]) -> i32 {
    let dir = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("onelf_extracted"));
    let patterns: Vec<String> = args
        .iter()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let paths: Vec<String> = (0..pkg.manifest.entries.len())
        .map(|i| pkg.manifest.entry_path(i))
        .collect();
    let missing: Vec<&str> = patterns
        .iter()
        .filter(|p| !paths.iter().any(|path| matches(p, path)))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        eprintln!("onelf-rt: not in the package: {}", missing.join(", "));
        return 1;
    }
    let keep = |rel: &Path| {
        patterns.is_empty() || {
            let rel = rel.to_string_lossy();
            patterns.iter().any(|p| matches(p, &rel))
        }
    };
    if let Err(e) =
        std::fs::create_dir_all(&dir).and_then(|()| crate::cache::extract_matching(pkg, &dir, keep))
    {
        eprintln!("onelf-rt: extraction failed: {e}");
        return 1;
    }
    eprintln!("Extracted to {}", dir.display());
    0
}

/// Write end of the pipe the stop signals write to, or `-1`.
static STOP_PIPE: AtomicI32 = AtomicI32::new(-1);

unsafe extern "C" {
    fn __onelf_signal_restorer();
}

unsafe extern "C" fn stop(_sig: core::ffi::c_int) {
    let fd = STOP_PIPE.load(Ordering::Relaxed);
    if fd >= 0 {
        let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
        let _ = rustix::io::write(fd, b"s");
    }
}

/// Make SIGINT, SIGTERM and SIGHUP readable on the returned pipe instead
/// of ending the process, so the mount is taken down first.
fn stop_pipe() -> Option<(OwnedFd, OwnedFd)> {
    use rustix::process::Signal;
    use rustix::runtime::{KernelSigSet, KernelSigaction, KernelSigactionFlags, kernel_sigaction};

    let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).ok()?;
    STOP_PIPE.store(rustix::fd::AsRawFd::as_raw_fd(&write), Ordering::Relaxed);
    for sig in [Signal::INT, Signal::TERM, Signal::HUP] {
        let action = KernelSigaction {
            sa_handler_kernel: Some(stop),
            sa_flags: KernelSigactionFlags::RESTORER,
            sa_restorer: Some(__onelf_signal_restorer),
            sa_mask: KernelSigSet::empty(),
        };
        unsafe {
            let _ = kernel_sigaction(sig, Some(action));
        }
    }
    Some((read, write))
}

/// Serve the package at a mountpoint of its own until a signal arrives.
///
/// The setuid helper is tried first, since its mount is one every process
/// of the user can see. A namespace mount is visible only through this
/// process, so the path printed reaches it through `/proc`.
fn mount(pkg: &mut PackageData) -> i32 {
    use crate::fuse::mount as fmount;

    let Some(claim) =
        crate::paths::create_launch_dir(pkg.manifest.name(), &pkg.manifest.header.package_id)
    else {
        eprintln!("onelf-rt: no private runtime directory to mount in");
        return 1;
    };
    let dir = claim.path().to_path_buf();
    let (fuse_fd, shown, private) = match fmount::fuse_mount(&dir) {
        Ok(fd) => (fd, dir.clone(), false),
        Err(helper_err) => match fmount::fuse_mount_unshare(&dir) {
            Ok(fd) => {
                let via_proc = Path::new("/proc")
                    .join(std::process::id().to_string())
                    .join("root")
                    .join(dir.strip_prefix("/").unwrap_or(&dir));
                (fd, via_proc, true)
            }
            Err(ns_err) => {
                eprintln!("onelf-rt: cannot mount: {helper_err}; {ns_err}");
                claim.release();
                return 1;
            }
        },
    };
    let Some((stop_read, _stop_write)) = stop_pipe() else {
        eprintln!("onelf-rt: cannot set up signal handling");
        fmount::fuse_unmount(&dir);
        claim.release();
        return 1;
    };

    println!("{}", shown.display());
    eprintln!("onelf-rt: serving the package there; interrupt to unmount");

    let mut state = crate::fuse::fs::FuseState::new(
        &pkg.manifest,
        &mut pkg.file,
        &pkg.footer,
        pkg.dict.as_deref(),
    );
    let mut buf = vec![0u8; 1024 * 1024 + 4096];
    state.run_loop(&fuse_fd, Some(&stop_read), None::<&OwnedFd>, &mut buf);
    drop(fuse_fd);
    if private {
        fmount::fuse_unmount_direct(&dir);
    } else {
        fmount::fuse_unmount(&dir);
    }
    claim.release();
    0
}

#[cfg(test)]
mod tests {
    use super::{info_value, matches};

    #[test]
    fn a_pattern_names_a_path_or_a_directory_above_it() {
        assert!(matches("bin/app", "bin/app"));
        assert!(matches("bin", "bin/app"));
        assert!(matches("./bin/", "bin/app"));
        assert!(!matches("bin", "binary/app"));
        assert!(!matches("bin/ap", "bin/app"));
        assert!(matches("lib/*.so*", "lib/libz.so.1"));
        assert!(matches("*.desktop", "share/applications/app.desktop"));
        assert!(matches("share/*/app", "share/icons/app/x.png"));
        assert!(!matches("*.png", "share/app.svg"));
        assert!(matches("*", "anything/at/all"));
    }

    #[test]
    fn package_info_values_are_unquoted() {
        let info = "name = \"My \\\"App\\\"\"\nversion = \"1.2\"\n";
        assert_eq!(info_value(info, "version").as_deref(), Some("1.2"));
        assert_eq!(info_value(info, "name").as_deref(), Some("My \"App\""));
        assert_eq!(info_value(info, "license"), None);
    }
}

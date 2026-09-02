//! Ephemeral tmpfs-based execution mode.
//!
//! When FUSE is unavailable, we can still run invisibly: enter a private
//! user+mount namespace, mount a tmpfs, extract the package into it, and
//! exec the target. The tmpfs is invisible to the host and is torn down
//! automatically by the kernel when the last process in the namespace
//! exits - no cleanup code, no files left on disk.
//!
//! Trade-off vs. FUSE: the whole package sits in RAM (vs. on-demand).
//! Fine for AppImage-scale bundles, use FUSE for very large packages.

use std::path::Path;

use crate::fuse::mount;
use crate::loader::PackageData;

/// Compute the tmpfs size needed to hold the extracted package.
/// Adds 25% headroom for filesystem overhead, minimum 8 MB.
pub(crate) fn tmpfs_size_for(pkg: &PackageData) -> u64 {
    let content_size: u64 = pkg
        .manifest
        .entries
        .iter()
        .flat_map(|e| e.blocks.iter())
        .map(|b| b.original_size)
        .sum();
    let with_overhead = content_size + content_size / 4;
    with_overhead.max(8 * 1024 * 1024)
}

/// Whether `needed` bytes plausibly fit in memory, read from `MemAvailable`.
///
/// A tmpfs lives in RAM, so a package larger than what is available would
/// mount fine and then fail partway through extraction. Checking first keeps
/// that failure on the recoverable side of the namespace boundary, where the
/// caller can still fall back to on-disk extraction. An unreadable or
/// unparseable `meminfo` is treated as "no objection", leaving behaviour
/// unchanged on kernels that do not report it.
fn fits_in_memory(needed: u64) -> bool {
    let Ok(info) = std::fs::read_to_string("/proc/meminfo") else {
        return true;
    };
    let available_kb = info
        .lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse::<u64>().ok());
    match available_kb {
        Some(kb) => needed <= kb.saturating_mul(1024),
        None => true,
    }
}

/// Execute the package in an ephemeral tmpfs mount. Never returns on success.
/// Returns `false` if setup fails and the caller should fall back further.
pub fn execute_tmpfs(
    pkg: &mut PackageData,
    ep_idx: usize,
    argv0: &str,
    exec_path: &str,
    args: &[String],
    interp_data: Option<&[u8]>,
    env_data: Option<&[u8]>,
) -> bool {
    crate::paths::sweep_stale_mountpoints();

    // Held through the exec below, so a concurrently starting instance
    // cannot reclaim this directory while the app is using it.
    let claim =
        match crate::paths::create_mountpoint(pkg.manifest.name(), &pkg.manifest.header.package_id)
        {
            Some(m) => m,
            None => return false,
        };
    let mountpoint = claim.path().to_path_buf();

    let size = tmpfs_size_for(pkg);
    if !fits_in_memory(size) {
        let _ = std::fs::remove_dir(&mountpoint);
        return false;
    }

    // Unsharing is the point of no return: the caller's fallback to cache
    // mode would otherwise run inside a user namespace it never asked for,
    // which is exactly what `needs-setuid` exists to avoid. Everything that
    // can fail and still be recovered from happens above this line; past it,
    // a failure is terminal.
    if let Err(e) = mount::enter_namespace() {
        eprintln!("onelf-rt: tmpfs: enter_namespace failed: {e}");
        let _ = std::fs::remove_dir(&mountpoint);
        return false;
    }

    if let Err(e) = mount::mount_tmpfs(&mountpoint, size) {
        eprintln!("onelf-rt: tmpfs: mount failed: {e}");
        let _ = std::fs::remove_dir(&mountpoint);
        std::process::exit(1);
    }

    if let Err(e) = crate::cache::extract_direct(pkg, &mountpoint) {
        eprintln!("onelf-rt: tmpfs: extract failed: {e}");
        std::process::exit(1);
    }

    let exe_path = Path::new(exec_path);
    let exe_dir = exe_path.parent().unwrap_or(Path::new("/"));
    let exe_name = exe_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("onelf");
    crate::portable::setup_portable(exe_dir, exe_name);

    // Exec directly: no fork, no FUSE server. When this process exits
    // the namespace is released and the tmpfs disappears with it.
    crate::launch::exec(&crate::launch::Launch {
        pkg,
        pkg_root: &mountpoint,
        mode: "tmpfs",
        ep_idx,
        argv0,
        exec_path,
        args,
        interp_data,
        env_data,
        private_ns: true,
        tree_writable: false,
    })
}

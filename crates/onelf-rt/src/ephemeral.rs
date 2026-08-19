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
fn tmpfs_size_for(pkg: &PackageData) -> u64 {
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
    use std::os::unix::process::CommandExt;

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

    // Resolve entrypoint and set up environment.
    let ep_target_entry = pkg.manifest.entrypoints[ep_idx].target_entry as usize;
    let ep_working_dir = pkg.manifest.entrypoints[ep_idx].working_dir;
    let ep_name = pkg
        .manifest
        .get_string(pkg.manifest.entrypoints[ep_idx].name)
        .to_string();
    let target_path_str = pkg.manifest.entry_path(ep_target_entry);
    let target_path = mountpoint.join(&target_path_str);
    let mountpoint_str = mountpoint.to_str().unwrap_or("").to_string();
    let lib_paths_str = pkg.manifest.lib_dirs().join(":");

    let exe_path = Path::new(exec_path);
    let exe_dir = exe_path.parent().unwrap_or(Path::new("/"));
    let exe_name = exe_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("onelf");
    crate::portable::setup_portable(exe_dir, exe_name);

    let target_path_s = target_path.to_str().unwrap_or("");
    let lib_path = crate::env::setup_env(
        &mountpoint_str,
        argv0,
        exec_path,
        &ep_name,
        "tmpfs",
        &lib_paths_str,
        target_path_s,
    );
    if let Some(data) = env_data {
        crate::env::apply_custom_env(data, &mountpoint_str);
    }

    // Working dir.
    match ep_working_dir {
        onelf_format::WorkingDir::PackageRoot => {
            let _ = std::env::set_current_dir(&mountpoint);
        }
        onelf_format::WorkingDir::EntrypointParent => {
            if let Some(parent) = target_path.parent() {
                let _ = std::env::set_current_dir(parent);
            }
        }
        onelf_format::WorkingDir::Inherit => {}
    }

    // Exec directly: no fork, no FUSE server. When this process exits
    // the namespace is released and the tmpfs disappears with it.
    let lib_dirs = pkg.manifest.lib_dirs();
    let bundled_interp_rel = interp_data.and_then(crate::interp::parse_bundled_interp_rel);

    if let Some(interp) =
        crate::interp::should_use_userland_exec(&target_path, &mountpoint, bundled_interp_rel)
    {
        crate::interp::exec_userland(&target_path, &interp, &lib_path, argv0, args);
    }

    let mut cmd = crate::interp::build_exec_command(
        &target_path,
        &mountpoint,
        &lib_dirs,
        &lib_path,
        true, // tmpfs mode: in private namespace
        argv0,
        args,
    );
    let err = cmd.exec();
    eprintln!("onelf-rt: tmpfs: exec failed: {err}");
    false
}

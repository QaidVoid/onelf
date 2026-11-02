//! Runtime-directory execution mode.
//!
//! The mode for a host that offers neither user namespaces nor a FUSE
//! helper. The package is extracted into the private per-user directory,
//! which on a systemd host is a RAM-backed tmpfs cleared at logout, and
//! removed again once the last process using it is gone. Nothing here
//! needs a namespace, a helper, or the persistent cache.
//!
//! The tree is an ephemeral execution view of this launch alone: it is
//! built in a claimed staging tree and moved onto the tree's name in one
//! rename, so the application only ever sees the complete view, and the
//! claim lock is the lease the launch's guardian holds against the
//! sweep. Its library links are this launch's own, so a host library the
//! resolver chose can be placed over its bundled copy with a plain
//! symlink, and the per-view environment input is written to the tree
//! before the application starts.

use std::ffi::OsString;
use std::path::Path;
use std::sync::atomic::Ordering;

use crate::loader::PackageData;

/// Execute the package from the private runtime directory. Never returns
/// on success. Returns `false` when the directory cannot be prepared and
/// the caller should try the next mode.
pub fn execute_rundir(
    pkg: &mut PackageData,
    ep_idx: usize,
    argv0: &str,
    exec_path: &str,
    args: &[OsString],
    interp_data: Option<&[u8]>,
    env_data: Option<&[u8]>,
) -> bool {
    crate::paths::sweep_stale_mountpoints();

    // Claimed before anything exists to lose, and taken along by the
    // guardian, which holds the claim itself for as long as the tree is
    // alive: a concurrent launch never sweeps a tree in use, however
    // thoroughly a descendant of the application discards what it
    // inherited.
    let claim =
        match crate::paths::create_launch_dir(pkg.manifest.name(), &pkg.manifest.header.package_id)
        {
            Some(m) => m,
            None => return false,
        };
    let tree = claim.path().to_path_buf();

    // Filled beside the tree and moved onto it in one rename, so the
    // tree the application runs from is complete from the first moment
    // it is visible. The staging tree is claimed like the tree, so a
    // launch killed mid-build leaves it to the next launch's sweep.
    let stage = match crate::paths::create_launch_stage(
        pkg.manifest.name(),
        &pkg.manifest.header.package_id,
    ) {
        Some(m) => m,
        None => {
            claim.release();
            return false;
        }
    };

    let needed = crate::ephemeral::tmpfs_size_for(pkg);
    if let Some(free) = free_space(&tree)
        && free < needed
    {
        eprintln!(
            "onelf-rt: rundir: {} has {} MB free, the package needs {} MB",
            tree.display(),
            free >> 20,
            needed >> 20
        );
        let _ = std::fs::remove_dir(stage.path());
        stage.release();
        claim.release();
        return false;
    }
    if let Err(e) = crate::cache::extract_direct(pkg, stage.path()) {
        eprintln!("onelf-rt: rundir: extract failed: {e}");
        let _ = std::fs::remove_dir_all(stage.path());
        stage.release();
        claim.release();
        return false;
    }
    if let Err(e) = std::fs::rename(stage.path(), &tree) {
        eprintln!("onelf-rt: rundir: publish failed: {e}");
        let _ = std::fs::remove_dir_all(stage.path());
        stage.release();
        claim.release();
        return false;
    }
    stage.release();

    let exe_path = Path::new(exec_path);
    let exe_dir = exe_path.parent().unwrap_or(Path::new("/"));
    let exe_name = exe_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("onelf");
    crate::portable::setup_portable(exe_dir, exe_name);

    // The guardian owns the launch from here: it holds the claim, forks
    // the application, reports the application's exit status on the
    // returned pipe the moment it is reaped, adopts the descendants a
    // daemonization leaves behind, and removes the tree once a drained
    // `wait` proves its subtree empty. This process returns as soon as the
    // report arrives, while the guardian, detached by then, outlives it
    // for exactly as long as the descendants do.
    let supervision = match crate::guardian::spawn(
        claim,
        || {
            crate::launch::exec(&crate::launch::Launch {
                pkg,
                pkg_root: &tree,
                mode: "rundir",
                ep_idx,
                argv0,
                exec_path,
                args,
                interp_data,
                env_data,
                private_ns: false,
                tree_writable: true,
                links: crate::launch::Links::Tree,
                resolved: None,
            })
        },
        |claim| remove_tree(claim.path()),
    ) {
        Ok(supervision) => supervision,
        Err(e) => {
            eprintln!("onelf-rt: rundir: fork failed: {e}");
            remove_tree(&tree);
            std::process::exit(1);
        }
    };

    // Signals meant for this process still reach the application, one hop
    // further along: this forwards to the guardian, and the guardian
    // forwards to the application.
    crate::fuse::CHILD_PID.store(supervision.pid as i32, Ordering::Relaxed);
    crate::fuse::install_signal_handlers();

    match crate::guardian::receive(&supervision.status) {
        Ok(outcome) => {
            if outcome == crate::guardian::Outcome::Unavailable {
                eprintln!("onelf-rt: rundir: the lifetime guardian could not be established");
            }
            std::process::exit(outcome.exit_code())
        }
        Err(e) => {
            eprintln!("onelf-rt: rundir: guardian status: {e}");
            std::process::exit(1);
        }
    }
}

/// Bytes available to this user on the filesystem holding `path`.
pub(crate) fn free_space(path: &Path) -> Option<u64> {
    rustix::fs::statvfs(path)
        .ok()
        .map(|s| s.f_bavail.saturating_mul(s.f_frsize))
}

fn remove_tree(tree: &Path) {
    let _ = std::fs::remove_dir_all(tree);
}

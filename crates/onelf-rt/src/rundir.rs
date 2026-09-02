//! Runtime-directory execution mode.
//!
//! The mode for a host that offers neither user namespaces nor a FUSE
//! helper. The package is extracted into the private per-user directory,
//! which on a systemd host is a RAM-backed tmpfs cleared at logout, and
//! removed again once the last process using it is gone. Nothing here
//! needs a namespace, a helper, or the persistent cache.
//!
//! The tree is this launch's own, so a host library the resolver chose can
//! be placed over its bundled copy with a plain symlink.

use std::path::Path;
use std::sync::atomic::Ordering;

use rustix::event::{PollFd, PollFlags, poll};
use rustix::io::FdFlags;
use rustix::process::{WaitOptions, waitpid};
use rustix::runtime::{Fork, kernel_fork};

use crate::loader::PackageData;

/// Execute the package from the private runtime directory. Never returns
/// on success. Returns `false` when the directory cannot be prepared and
/// the caller should try the next mode.
pub fn execute_rundir(
    pkg: &mut PackageData,
    ep_idx: usize,
    argv0: &str,
    exec_path: &str,
    args: &[String],
    interp_data: Option<&[u8]>,
    env_data: Option<&[u8]>,
) -> bool {
    crate::paths::sweep_stale_mountpoints();

    // Held by this process for as long as the tree exists, and inherited by
    // everything it forks, so a concurrent launch never sweeps a tree in
    // use. Once every holder is gone the sweep removes what is left.
    let claim =
        match crate::paths::create_mountpoint(pkg.manifest.name(), &pkg.manifest.header.package_id)
        {
            Some(m) => m,
            None => return false,
        };
    let tree = claim.path().to_path_buf();

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
        let _ = std::fs::remove_dir(&tree);
        return false;
    }
    if let Err(e) = crate::cache::extract_direct(pkg, &tree) {
        eprintln!("onelf-rt: rundir: extract failed: {e}");
        remove_tree(&tree);
        return false;
    }

    let exe_path = Path::new(exec_path);
    let exe_dir = exe_path.parent().unwrap_or(Path::new("/"));
    let exe_name = exe_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("onelf");
    crate::portable::setup_portable(exe_dir, exe_name);

    // The write end is inherited by the app and everything it forks; the
    // read end reports a hangup once the last of them has gone.
    let (pipe_read, pipe_write) = match rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("onelf-rt: rundir: pipe: {e}");
            remove_tree(&tree);
            return false;
        }
    };
    let _ = rustix::io::fcntl_setfd(&pipe_write, FdFlags::empty());

    match unsafe { kernel_fork() } {
        Ok(Fork::Child(_)) => {
            drop(pipe_read);
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
            })
        }
        Ok(Fork::ParentOf(child)) => {
            drop(pipe_write);
            crate::fuse::CHILD_PID.store(child.as_raw_nonzero().get(), Ordering::Relaxed);
            crate::fuse::install_signal_handlers();

            let status = loop {
                match waitpid(Some(child), WaitOptions::empty()) {
                    Ok(Some((_, status))) => break status,
                    Ok(None) | Err(rustix::io::Errno::INTR) => continue,
                    Err(e) => {
                        eprintln!("onelf-rt: rundir: wait: {e}");
                        remove_tree(&tree);
                        std::process::exit(1);
                    }
                }
            };

            // An app that daemonizes leaves the tree in use after the
            // foreground exits. Removing it then would pull the files out
            // from under the survivor, so a detached reaper waits for the
            // last user instead and the launcher returns promptly.
            if tree_quiet(&pipe_read, &tree, 0) {
                remove_tree(&tree);
            } else {
                match unsafe { kernel_fork() } {
                    Ok(Fork::Child(_)) => {
                        let _ = rustix::process::setsid();
                        detach_stdio();
                        while !tree_quiet(&pipe_read, &tree, 500) {}
                        remove_tree(&tree);
                        std::process::exit(0);
                    }
                    Ok(Fork::ParentOf(_)) => {}
                    Err(_) => remove_tree(&tree),
                }
            }

            if let Some(code) = status.exit_status() {
                std::process::exit(code)
            } else if let Some(sig) = status.terminating_signal() {
                std::process::exit(128 + sig)
            } else {
                std::process::exit(1)
            }
        }
        Err(e) => {
            eprintln!("onelf-rt: rundir: fork failed: {e}");
            remove_tree(&tree);
            std::process::exit(1);
        }
    }
}

/// Whether nothing is using the tree any more: every holder of the death
/// pipe's write end has gone, and no process in this namespace runs out
/// of it. Waits up to `timeout_ms` for the pipe.
fn tree_quiet(pipe_read: &rustix::fd::OwnedFd, tree: &Path, timeout_ms: u32) -> bool {
    let mut fds = [PollFd::new(pipe_read, PollFlags::IN)];
    let timeout = rustix::event::Timespec {
        tv_sec: (timeout_ms / 1000) as _,
        tv_nsec: ((timeout_ms % 1000) * 1_000_000) as _,
    };
    let hung_up = match poll(&mut fds, Some(&timeout)) {
        Ok(_) => fds[0].revents().contains(PollFlags::HUP),
        Err(_) => false,
    };
    hung_up && !crate::fuse::mount_in_use(tree)
}

/// Bytes available to this user on the filesystem holding `path`.
fn free_space(path: &Path) -> Option<u64> {
    rustix::fs::statvfs(path)
        .ok()
        .map(|s| s.f_bavail.saturating_mul(s.f_frsize))
}

fn remove_tree(tree: &Path) {
    let _ = std::fs::remove_dir_all(tree);
}

/// Let go of the launcher's standard streams, so a caller reading from a
/// pipe sees it close when the app is done rather than when the reaper is.
fn detach_stdio() {
    use std::os::fd::FromRawFd;
    let Ok(null) = rustix::fs::open(
        "/dev/null",
        rustix::fs::OFlags::RDWR,
        rustix::fs::Mode::empty(),
    ) else {
        return;
    };
    for target in 0..=2 {
        let mut slot =
            std::mem::ManuallyDrop::new(unsafe { std::os::fd::OwnedFd::from_raw_fd(target) });
        let _ = rustix::io::dup2(&null, &mut slot);
    }
}

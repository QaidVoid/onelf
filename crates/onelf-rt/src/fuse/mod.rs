//! FUSE-based execution mode.
//!
//! Mounts the package contents as a read-only FUSE filesystem and executes
//! the entrypoint directly from the mount. The parent process serves FUSE
//! requests while the child runs the target binary. A death pipe detects
//! child exit for reliable cleanup.

pub(crate) mod fs;
pub(crate) mod mount;
mod protocol;

use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};

use rustix::io::FdFlags;
use rustix::process::{Pid, Signal, WaitOptions, kill_process, waitpid};
use rustix::runtime::{KernelSigSet, KernelSigaction, KernelSigactionFlags, kernel_sigaction};

use crate::loader::PackageData;

static CHILD_PID: AtomicI32 = AtomicI32::new(0);

/// Write end of the pipe the `SIGCHLD` handler writes a byte to, so the event
/// loop can wait on the launched process exiting rather than only on the death
/// pipe. `poll` cannot wait on a signal, so the signal has to become readable
/// data. `-1` means no pipe is installed and the write is skipped.
static SIGCHLD_PIPE: AtomicI32 = AtomicI32::new(-1);

// Signal restorer -- required because kernel_sigaction bypasses libc.
// x86_64 Linux requires SA_RESTORER for signal handler return to work.
// aarch64 Linux handles signal return in the kernel, no restorer needed.
#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".global __onelf_signal_restorer",
    ".type __onelf_signal_restorer, @function",
    "__onelf_signal_restorer:",
    "mov rax, 15", // __NR_rt_sigreturn
    "syscall",
);

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".global __onelf_signal_restorer",
    ".type __onelf_signal_restorer, @function",
    "__onelf_signal_restorer:",
    "mov x8, #139", // __NR_rt_sigreturn
    "svc #0",
);

// i386 Linux also requires SA_RESTORER. Without SA_SIGINFO the kernel builds
// the legacy (non-rt) sigframe, so the restorer pops the signal number and
// calls sigreturn (not rt_sigreturn).
#[cfg(target_arch = "x86")]
core::arch::global_asm!(
    ".global __onelf_signal_restorer",
    ".type __onelf_signal_restorer, @function",
    "__onelf_signal_restorer:",
    "pop eax",      // discard the signal number pushed on the legacy frame
    "mov eax, 119", // __NR_sigreturn
    "int 0x80",
);

unsafe extern "C" {
    fn __onelf_signal_restorer();
}

unsafe extern "C" fn signal_handler(sig: core::ffi::c_int) {
    if sig == 17 {
        // SIGCHLD: wake the event loop so it can see the launched process go.
        // Waiting only for the death pipe means waiting for every descendant,
        // and a launcher that does not return until a daemon its child started
        // has finished is a launcher that reports that daemon dead. Writing a
        // byte is the whole handler, which keeps it async-signal-safe.
        let fd = SIGCHLD_PIPE.load(Ordering::Relaxed);
        if fd >= 0 {
            let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
            let _ = rustix::io::write(borrowed, b"c");
        }
        return;
    }
    // Forward other signals to child
    let pid = CHILD_PID.load(Ordering::Relaxed);
    if pid > 0
        && let Some(pid) = Pid::from_raw(pid)
        && let Some(signal) = Signal::from_named_raw(sig)
    {
        let _ = kill_process(pid, signal);
    }
}

fn install_signal_handlers() {
    let mut mask = KernelSigSet::empty();
    mask.insert(Signal::INT);
    mask.insert(Signal::TERM);
    mask.insert(Signal::HUP);
    mask.insert(Signal::QUIT);

    let flags = KernelSigactionFlags::RESTORER;

    for &sig in &[
        Signal::INT,
        Signal::TERM,
        Signal::HUP,
        Signal::QUIT,
        Signal::CHILD,
    ] {
        let action = KernelSigaction {
            sa_handler_kernel: Some(signal_handler),
            sa_flags: flags,
            sa_restorer: Some(__onelf_signal_restorer),
            sa_mask: mask.clone(),
        };
        unsafe {
            let _ = kernel_sigaction(sig, Some(action));
        }
    }
}

/// Check if a path is currently a mountpoint by reading /proc/self/mountinfo.
/// This avoids stat/exists calls that can hang on dead FUSE mounts.
fn is_mountpoint(path: &Path) -> bool {
    let target = path.to_string_lossy();
    let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    // Field 5 (0-indexed: 4) in mountinfo is the mount point
    info.lines().any(|line| {
        line.split(' ')
            .nth(4)
            .map(|mp| mp.replace("\\040", " ") == *target)
            .unwrap_or(false)
    })
}

/// Execute directly from an existing FUSE mount (another instance is serving).
/// This process becomes the child, so no fork or FUSE loop is needed.
// Threads the launch context straight to the exec call; grouping it
// would add a type used at one call site.
#[allow(clippy::too_many_arguments)]
fn exec_from_mount(
    pkg: &mut PackageData,
    ep_idx: usize,
    argv0: &str,
    exec_path: &str,
    args: &[String],
    interp_data: Option<&[u8]>,
    env_data: Option<&[u8]>,
    mountpoint: &Path,
) -> bool {
    let exe_path = std::path::Path::new(exec_path);
    let exe_dir = exe_path.parent().unwrap_or(std::path::Path::new("."));
    let exe_name = exe_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("onelf");
    crate::portable::setup_portable(exe_dir, exe_name);

    crate::launch::exec(&crate::launch::Launch {
        pkg,
        pkg_root: mountpoint,
        mode: "fuse",
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

/// Whether any process other than this one is still running out of `mountpoint`.
///
/// The death pipe cannot answer this. A process that daemonizes closes every
/// descriptor it inherited, so the pipe hangs up while the process is very
/// much alive and still reading from the mount. `/proc` is the only remaining
/// witness: a process launched from the package has its executable, and often
/// its working directory, inside the mount.
///
/// Only processes in this PID namespace are visible, which is exactly the set
/// that could be using a mount private to it. A `/proc` that cannot be read
/// reports "not in use", leaving the previous teardown behaviour in place
/// rather than holding a mount forever on a guess.
fn mount_in_use(mountpoint: &Path) -> bool {
    let me = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if pid == me {
            continue;
        }
        let proc_dir = entry.path();
        // Unreadable links belong to other users' processes, which cannot be
        // running out of a mount private to this namespace.
        for link in ["exe", "cwd", "root"] {
            if let Ok(target) = std::fs::read_link(proc_dir.join(link))
                && target.starts_with(mountpoint)
            {
                return true;
            }
        }
    }
    false
}

fn cleanup_mountpoint(mountpoint: &Path, used_namespace: bool) {
    if used_namespace {
        mount::fuse_unmount_direct(mountpoint);
    } else {
        mount::fuse_unmount(mountpoint);
    }
    let _ = std::fs::remove_dir(mountpoint);
}

/// Execute the package via FUSE mount.
///
/// On success, exits the process with the child's exit code (never returns).
/// Returns `false` if FUSE is unavailable and caller should fall back.
// Threads the launch context straight to the exec call; grouping it
// would add a type used at one call site.
#[allow(clippy::too_many_arguments)]
pub fn execute_fuse(
    pkg: &mut PackageData,
    ep_idx: usize,
    argv0: &str,
    exec_path: &str,
    args: &[String],
    interp_data: Option<&[u8]>,
    env_data: Option<&[u8]>,
    needs_setuid: bool,
) -> bool {
    crate::paths::sweep_stale_mountpoints();

    // Held by the parent, which serves the filesystem and outlives the
    // child, so no concurrent sweep can reclaim the directory.
    let claim =
        match crate::paths::create_mountpoint(pkg.manifest.name(), &pkg.manifest.header.package_id)
        {
            Some(m) => m,
            None => return false,
        };
    let mountpoint = claim.path().to_path_buf();

    // If already mounted by another instance, reuse it and exec directly.
    // (Only reachable via the fusermount3 path; namespace mounts are private.)
    if is_mountpoint(&mountpoint) {
        if mountpoint.read_dir().is_ok() {
            return exec_from_mount(
                pkg,
                ep_idx,
                argv0,
                exec_path,
                args,
                interp_data,
                env_data,
                &mountpoint,
            );
        }
        // Dead mount (FUSE daemon exited). Clean up and proceed with fresh mount.
        mount::fuse_unmount(&mountpoint);
    }

    // Prefer the namespace-based mount. No external helper, private to us,
    // tears down automatically on exit. Fall back to fusermount3 if the
    // kernel disallows unprivileged user namespaces (e.g. restricted distros).
    //
    // Setting `ONELF_FUSE_NO_NAMESPACE=1` forces the fusermount3 path,
    // which is needed for packages that expect to stay in the host's
    // user namespace. The specific use case is rootless podman /
    // distrobox: they rely on setuid `newuidmap` / `newgidmap` to
    // build their own nested user namespace, and setuid bits do not
    // survive a CLONE_NEWUSER unshare. Staying in the host userns
    // keeps those helpers working.
    //
    // A package built with `needs-setuid` asks for the same thing for its own
    // reason: it runs sudo or pkexec, and a setuid bit does nothing inside a
    // namespace we made ourselves.
    let skip_namespace = needs_setuid
        || std::env::var_os("ONELF_FUSE_NO_NAMESPACE")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false);

    let ns_result = if skip_namespace {
        let why = if needs_setuid {
            "package needs setuid; using fusermount3"
        } else {
            "ONELF_FUSE_NO_NAMESPACE set; using fusermount3"
        };
        Err(std::io::Error::other(why))
    } else {
        mount::fuse_mount_unshare(&mountpoint)
    };

    let (fuse_fd, used_namespace) = match ns_result {
        Ok(fd) => (fd, true),
        Err(ns_err) => {
            if !mount::fusermount3_available() {
                // Expected where the package asked to stay in the host's
                // namespace: without fusermount3 there is no way to mount
                // outside one, so extraction takes over. Not a failure.
                if !needs_setuid {
                    eprintln!("onelf-rt: fuse: namespace mount failed: {ns_err}");
                    eprintln!("onelf-rt: fuse: fusermount3 not available either; cannot continue");
                }
                let _ = std::fs::remove_dir(&mountpoint);
                return false;
            }
            match mount::fuse_mount(&mountpoint) {
                Ok(fd) => (fd, false),
                Err(e) => {
                    eprintln!("onelf-rt: fuse: mount failed: {e}");
                    let _ = std::fs::remove_dir(&mountpoint);
                    return false;
                }
            }
        }
    };
    // Set CLOEXEC so child doesn't inherit the FUSE fd after exec.
    let _ = rustix::io::fcntl_setfd(&fuse_fd, FdFlags::CLOEXEC);

    // Set up portable directories (doesn't access FUSE mount)
    let exe_path = std::path::Path::new(exec_path);
    let exe_dir = exe_path.parent().unwrap_or(std::path::Path::new("."));
    let exe_name = exe_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("onelf");
    crate::portable::setup_portable(exe_dir, exe_name);

    // Death pipe: when the child (and all its descendants) exit, the write end
    // closes and poll() on the read end returns POLLHUP.
    let (pipe_read, pipe_write) = match rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC) {
        Ok(p) => p,
        Err(_) => {
            cleanup_mountpoint(&mountpoint, used_namespace);
            return false;
        }
    };
    // Remove CLOEXEC from write end so the exec'd child inherits it.
    let _ = rustix::io::fcntl_setfd(&pipe_write, FdFlags::empty());

    use rustix::runtime::{Fork, kernel_fork};

    match unsafe { kernel_fork() } {
        Ok(Fork::Child(_)) => {
            // Everything that reads the mount has to happen in the child,
            // after the fork: the parent's FUSE event loop is what serves
            // it, and that loop is only now starting.
            crate::launch::exec(&crate::launch::Launch {
                pkg,
                pkg_root: &mountpoint,
                mode: "fuse",
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
        Ok(Fork::ParentOf(child_pid)) => {
            // Close write end in parent -- only child holds it now
            drop(pipe_write);

            CHILD_PID.store(child_pid.as_raw_nonzero().get(), Ordering::Relaxed);

            // Installed before the handlers, so a child that exits immediately
            // still finds somewhere to record it.
            let sigchld = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).ok();
            if let Some((_, w)) = &sigchld {
                SIGCHLD_PIPE.store(rustix::fd::AsRawFd::as_raw_fd(w), Ordering::Relaxed);
            }

            install_signal_handlers();

            let mut state = fs::FuseState::new(
                &pkg.manifest,
                &mut pkg.file,
                &pkg.footer,
                pkg.dict.as_deref(),
            );

            let mut fuse_buf = vec![0u8; 1024 * 1024 + 4096];
            state.run_loop(
                &fuse_fd,
                &pipe_read,
                sigchld.as_ref().map(|(r, _)| r),
                &mut fuse_buf,
            );

            // Event loop exited -- reap child
            let exit_status = loop {
                match waitpid(Some(child_pid), WaitOptions::NOHANG) {
                    Ok(Some((_pid, status))) => break status,
                    Ok(None) => match waitpid(Some(child_pid), WaitOptions::empty()) {
                        Ok(Some((_pid, status))) => break status,
                        Ok(None) => continue,
                        Err(rustix::io::Errno::INTR) => continue,
                        Err(_) => {
                            cleanup_mountpoint(&mountpoint, used_namespace);
                            std::process::exit(1);
                        }
                    },
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(_) => {
                        cleanup_mountpoint(&mountpoint, used_namespace);
                        std::process::exit(1);
                    }
                }
            };

            // The death pipe hanging up does not mean the package is finished.
            // An app that daemonizes forks a background copy, lets the
            // foreground exit, and closes the descriptors it inherited, so the
            // pipe reports the same thing whether the work ended or moved into
            // the background. Tearing the filesystem down on that signal alone
            // leaves the surviving process blocked forever on its next read,
            // which is indistinguishable from the app having hung.
            //
            // So when something is still running out of the mount, the server
            // moves to a background process of its own and this one exits with
            // the child's status, keeping the caller's `waitpid` prompt.
            // Deciding here whether anything still needs the mount does not
            // work: a process that daemonizes has not necessarily forked by
            // the time the foreground exits, so the answer is a race, and
            // losing it tears the filesystem down under the process that was
            // about to appear. Hand off unconditionally and let the background
            // server ask once things have settled.
            let handed_off = match unsafe { kernel_fork() } {
                Ok(Fork::Child(_)) => {
                    // Leave the launcher's session so a Ctrl-C aimed at it
                    // does not take down the filesystem a daemon is still
                    // reading from.
                    let _ = rustix::process::setsid();
                    // Let go of the launcher's standard streams. Holding them
                    // keeps the write end of whatever pipe the caller set up
                    // open, so `$(app)`, `app | head` and every other reader
                    // would block until this server exits rather than until
                    // the app is done.
                    if let Ok(null) = rustix::fs::open(
                        "/dev/null",
                        rustix::fs::OFlags::RDWR,
                        rustix::fs::Mode::empty(),
                    ) {
                        for target in 0..=2 {
                            // `dup2` wants to own the descriptor it replaces,
                            // but 0, 1 and 2 are not ours to close: the dup is
                            // what puts /dev/null there, and dropping the
                            // wrapper afterwards would close it again.
                            use std::os::fd::FromRawFd;
                            let mut slot = std::mem::ManuallyDrop::new(unsafe {
                                std::os::fd::OwnedFd::from_raw_fd(target)
                            });
                            let _ = rustix::io::dup2(&null, &mut slot);
                        }
                    }
                    state.serve_detached(&fuse_fd, &mut fuse_buf, || !mount_in_use(&mountpoint));
                    drop(fuse_fd);
                    cleanup_mountpoint(&mountpoint, used_namespace);
                    std::process::exit(0);
                }
                Ok(Fork::ParentOf(_)) => true,
                // No second process to serve from, so the old behaviour is
                // all that is left: tear down and let any survivor fail.
                Err(_) => false,
            };
            if !handed_off {
                drop(fuse_fd);
                cleanup_mountpoint(&mountpoint, used_namespace);
            }

            if let Some(code) = exit_status.exit_status() {
                std::process::exit(code)
            } else if let Some(sig) = exit_status.terminating_signal() {
                unsafe {
                    let action = KernelSigaction {
                        sa_handler_kernel: None,
                        sa_flags: KernelSigactionFlags::RESTORER,
                        sa_restorer: Some(__onelf_signal_restorer),
                        sa_mask: KernelSigSet::empty(),
                    };
                    if let Some(signal) = Signal::from_named_raw(sig) {
                        let _ = kernel_sigaction(signal, Some(action));
                        let _ = kill_process(rustix::process::getpid(), signal);
                    }
                }
                std::process::exit(128 + sig)
            } else {
                std::process::exit(1)
            }
        }
        Err(e) => {
            cleanup_mountpoint(&mountpoint, used_namespace);
            eprintln!("onelf-rt: fork failed: {e}");
            std::process::exit(1);
        }
    }
}

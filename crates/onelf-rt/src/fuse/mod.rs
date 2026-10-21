//! FUSE-based execution mode.
//!
//! Mounts the package contents as a read-only FUSE filesystem and executes
//! the entrypoint directly from the mount. Concurrent launches of one
//! package share a mount and the server behind it (see [`share`]). A
//! launch that does not share serves its own mount from the parent process
//! while the child runs the target binary, and a death pipe detects child
//! exit for reliable cleanup.

pub(crate) mod fs;
pub(crate) use fs::TRACE_ARM;
pub(crate) mod mount;
mod protocol;
pub(crate) mod share;

use std::ffi::OsString;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};

use rustix::io::FdFlags;
use rustix::process::{Pid, Signal, WaitOptions, kill_process, waitpid};
use rustix::runtime::{KernelSigSet, KernelSigaction, KernelSigactionFlags, kernel_sigaction};

use crate::loader::PackageData;

pub(crate) static CHILD_PID: AtomicI32 = AtomicI32::new(0);

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

pub(crate) fn install_signal_handlers() {
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
        // Only an exit is worth waking the event loop for. Without
        // NOCLDSTOP a Ctrl-Z, a `kill -STOP`, or a debugger attaching also
        // raise SIGCHLD, and the loop would stop serving the mount for a
        // child that is about to resume and read from it.
        let sa_flags = if sig == Signal::CHILD {
            flags | KernelSigactionFlags::NOCLDSTOP
        } else {
            flags
        };
        let action = KernelSigaction {
            sa_handler_kernel: Some(signal_handler),
            sa_flags,
            sa_restorer: Some(__onelf_signal_restorer),
            sa_mask: mask.clone(),
        };
        unsafe {
            let _ = kernel_sigaction(sig, Some(action));
        }
    }
}

/// What a launch runs, carried unchanged from the caller to the exec.
pub(crate) struct Request<'a> {
    pub ep_idx: usize,
    pub argv0: &'a str,
    pub exec_path: &'a str,
    pub args: &'a [OsString],
    pub interp_data: Option<&'a [u8]>,
    pub env_data: Option<&'a [u8]>,
}

impl Request<'_> {
    /// The launch of this request from the package tree at `pkg_root`.
    fn launch<'b>(
        &'b self,
        pkg: &'b PackageData,
        pkg_root: &'b Path,
        private_ns: bool,
        links: crate::launch::Links<'b>,
    ) -> crate::launch::Launch<'b> {
        crate::launch::Launch {
            pkg,
            pkg_root,
            mode: "fuse",
            ep_idx: self.ep_idx,
            argv0: self.argv0,
            exec_path: self.exec_path,
            args: self.args,
            interp_data: self.interp_data,
            env_data: self.env_data,
            private_ns,
            tree_writable: false,
            links,
        }
    }

    /// Point the app at the portable directories beside the packed file.
    fn setup_portable(&self) {
        let exe_path = Path::new(self.exec_path);
        let exe_dir = exe_path.parent().unwrap_or(Path::new("."));
        let exe_name = exe_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("onelf");
        crate::portable::setup_portable(exe_dir, exe_name);
    }
}

/// Leave the launcher's session and let go of its standard streams, as a
/// server that outlives the launcher has to.
///
/// A Ctrl-C aimed at the launcher must not take down the filesystem a
/// daemon is still reading from. Holding the streams keeps the write end
/// of whatever pipe the caller set up open, so `$(app)`, `app | head` and
/// every other reader would block until the server exits rather than
/// until the app is done.
fn detach_from_caller() {
    let _ = rustix::process::setsid();
    if let Ok(null) = rustix::fs::open(
        "/dev/null",
        rustix::fs::OFlags::RDWR,
        rustix::fs::Mode::empty(),
    ) {
        for target in 0..=2 {
            // `dup2` wants to own the descriptor it replaces, but 0, 1 and 2
            // are not ours to close: the dup is what puts /dev/null there,
            // and dropping the wrapper afterwards would close it again.
            use std::os::fd::FromRawFd;
            let mut slot =
                std::mem::ManuallyDrop::new(unsafe { std::os::fd::OwnedFd::from_raw_fd(target) });
            let _ = rustix::io::dup2(&null, &mut slot);
        }
    }
}

/// Whether any process other than this one is still running out of `mountpoint`.
///
/// The death pipe cannot answer this. A process that daemonizes closes every
/// descriptor it inherited, so the pipe hangs up while the process is very
/// much alive and still reading from the mount. `/proc` is the only remaining
/// witness, read by [`tree_in_use`].
pub(crate) fn mount_in_use(mountpoint: &Path) -> bool {
    // A forced unmount (`fusermount3 -u -z`) leaves the directory, and any
    // process still sitting in it, in place: the path prefix matches even
    // though nothing is mounted there any more, and this scan would then
    // hold the dead mount open forever. Only a live mount has users.
    is_live_mount(mountpoint) && tree_in_use(mountpoint)
}

/// Whether any process other than this one runs out of the directory `dir`,
/// whether or not anything is mounted there.
///
/// A process launched from the package has its executable, and often its
/// working directory, inside it. One started by userland exec or under a
/// loader has neither: its executable is the packed file or the loader.
/// The files it has mapped from `dir` still give it away, so the maps are
/// read once the cheaper links have found nobody.
///
/// Only processes in this PID namespace are visible, which is exactly the set
/// that could be using a mount private to it. A `/proc` that cannot be read
/// reports "not in use", leaving the previous teardown behaviour in place
/// rather than holding a mount forever on a guess.
pub(crate) fn tree_in_use(dir: &Path) -> bool {
    let me = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    let others: Vec<std::path::PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
                .is_some_and(|pid| pid != me)
        })
        .map(|e| e.path())
        .collect();
    // Unreadable links and maps belong to other users' processes, which
    // cannot be running out of a directory private to this one.
    let linked = |proc_dir: &std::path::PathBuf| {
        ["exe", "cwd", "root"].iter().any(|link| {
            std::fs::read_link(proc_dir.join(link)).is_ok_and(|target| target.starts_with(dir))
        })
    };
    let mapped = |proc_dir: &std::path::PathBuf| {
        std::fs::read_to_string(proc_dir.join("maps")).is_ok_and(|maps| {
            maps.lines()
                .filter_map(|line| line.find('/').map(|at| &line[at..]))
                .any(|path| Path::new(path).starts_with(dir))
        })
    };
    others.iter().any(linked) || others.iter().any(mapped)
}

/// Whether `mountpoint` is still an actual mount.
///
/// `/proc/self/mounts` names mount points verbatim; onelf's mountpoint
/// names come from `name_stem`, which keeps only characters `/proc` does
/// not escape, so a plain text comparison is enough.
fn is_live_mount(mountpoint: &Path) -> bool {
    let Ok(mounts) = std::fs::read_to_string("/proc/self/mounts") else {
        // Unknowable: claim the mount is live so the /proc scan runs and
        // the previous behaviour stands.
        return true;
    };
    let mp = mountpoint.to_string_lossy();
    mounts
        .lines()
        .any(|l| l.split_whitespace().nth(1) == Some(mp.as_ref()))
}

fn cleanup_mountpoint(claim: crate::paths::Mountpoint, used_namespace: bool) {
    if used_namespace {
        mount::fuse_unmount_direct(claim.path());
    } else {
        mount::fuse_unmount(claim.path());
    }
    claim.release();
}

/// Whether this launch stays out of a user namespace of its own: the
/// package runs setuid binaries, or `ONELF_FUSE_NO_NAMESPACE` asks.
///
/// The variable is for packages that expect to stay in the host's user
/// namespace. The specific use case is rootless podman / distrobox: they
/// rely on setuid `newuidmap` / `newgidmap` to build their own nested user
/// namespace, and setuid bits do not survive a CLONE_NEWUSER unshare.
/// A package built with `needs-setuid` asks for the same thing for its
/// own reason: it runs sudo or pkexec.
fn skips_namespace(needs_setuid: bool) -> bool {
    needs_setuid
        || std::env::var_os("ONELF_FUSE_NO_NAMESPACE")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false)
}

/// Mount at `mountpoint` inside a private user and mount namespace this
/// process enters. No external helper, private to us, torn down by the
/// kernel on exit. The error says why it was not done.
fn mount_in_namespace(mountpoint: &Path, needs_setuid: bool) -> std::io::Result<OwnedFd> {
    if skips_namespace(needs_setuid) {
        let why = if needs_setuid {
            "package needs setuid; using fusermount3"
        } else {
            "ONELF_FUSE_NO_NAMESPACE set; using fusermount3"
        };
        return Err(std::io::Error::other(why));
    }
    mount::fuse_mount_unshare(mountpoint)
}

/// Mount at `mountpoint` through `fusermount3`, the fallback where the
/// kernel disallows unprivileged user namespaces or the launch stays out
/// of one. `ns_err` is why the namespace mount was not made.
fn mount_with_helper(
    mountpoint: &Path,
    ns_err: &std::io::Error,
    needs_setuid: bool,
) -> Option<OwnedFd> {
    if !mount::fusermount3_available() {
        // Expected where the package asked to stay in the host's
        // namespace: without fusermount3 there is no way to mount
        // outside one, so extraction takes over. Not a failure.
        if !needs_setuid {
            eprintln!("onelf-rt: fuse: namespace mount failed: {ns_err}");
            eprintln!("onelf-rt: fuse: fusermount3 not available either; cannot continue");
        }
        return None;
    }
    match mount::fuse_mount(mountpoint) {
        Ok(fd) => Some(fd),
        Err(e) => {
            eprintln!("onelf-rt: fuse: mount failed: {e}");
            None
        }
    }
}

/// Execute the package via FUSE mount.
///
/// On success, exits the process with the child's exit code (never returns).
/// Returns `false` if FUSE is unavailable and caller should fall back.
// The caller's arguments, grouped into a `Request` on the way in.
#[allow(clippy::too_many_arguments)]
pub fn execute_fuse(
    pkg: &mut PackageData,
    ep_idx: usize,
    argv0: &str,
    exec_path: &str,
    args: &[OsString],
    interp_data: Option<&[u8]>,
    env_data: Option<&[u8]>,
    needs_setuid: bool,
) -> bool {
    crate::paths::sweep_stale_mountpoints();

    let req = Request {
        ep_idx,
        argv0,
        exec_path,
        args,
        interp_data,
        env_data,
    };
    if share::wanted() {
        match share::launch(pkg, &req, needs_setuid) {
            share::Declined::Private => {}
            share::Declined::Unmountable => return false,
        }
    }
    execute_private(pkg, &req, needs_setuid)
}

/// Mount the package for this launch alone, serve it from this process
/// while the child runs the app, and hand the mount to a background
/// server if anything is still using it when the child exits.
fn execute_private(pkg: &mut PackageData, req: &Request, needs_setuid: bool) -> bool {
    // Held by the parent, which serves the filesystem and outlives the
    // child, so no concurrent sweep can reclaim the directory.
    let name = pkg.manifest.name();
    let id = &pkg.manifest.header.package_id;
    let Some(claim) = crate::paths::create_mountpoint(name, id) else {
        return false;
    };

    // Prefer the namespace mount. A helper mount is visible on the host,
    // so it goes in a directory of this launch's own: another launch of
    // the package that also does not share would otherwise stack its
    // mount on this one.
    let (claim, fuse_fd, used_namespace) = match mount_in_namespace(claim.path(), needs_setuid) {
        Ok(fd) => (claim, fd, true),
        Err(ns_err) => {
            claim.release();
            let Some(claim) = crate::paths::create_launch_dir(name, id) else {
                return false;
            };
            match mount_with_helper(claim.path(), &ns_err, needs_setuid) {
                Some(fd) => (claim, fd, false),
                None => {
                    claim.release();
                    return false;
                }
            }
        }
    };
    let mountpoint = claim.path().to_path_buf();
    // Set CLOEXEC so child doesn't inherit the FUSE fd after exec.
    let _ = rustix::io::fcntl_setfd(&fuse_fd, FdFlags::CLOEXEC);

    // Set up portable directories (doesn't access FUSE mount)
    req.setup_portable();

    // Death pipe: when the child (and all its descendants) exit, the write end
    // closes and poll() on the read end returns POLLHUP.
    let (pipe_read, pipe_write) = match rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC) {
        Ok(p) => p,
        Err(_) => {
            cleanup_mountpoint(claim, used_namespace);
            return false;
        }
    };
    // Remove CLOEXEC from write end so the exec'd child inherits it.
    let _ = rustix::io::fcntl_setfd(&pipe_write, FdFlags::empty());

    // The child resolves which libraries the mount shows from outside the
    // package, since only it reads the mount, and the parent serves them.
    let (links_tx, links_rx) = match crate::links::channel() {
        Ok(c) => c,
        Err(_) => {
            cleanup_mountpoint(claim, used_namespace);
            return false;
        }
    };

    use rustix::runtime::{Fork, kernel_fork};

    match unsafe { kernel_fork() } {
        Ok(Fork::Child(_)) => {
            drop(links_rx);
            // Everything that reads the mount has to happen in the child,
            // after the fork: the parent's FUSE event loop is what serves
            // it, and that loop is only now starting.
            crate::launch::exec(&req.launch(
                pkg,
                &mountpoint,
                used_namespace,
                crate::launch::Links::Server(&links_tx),
            ))
        }
        Ok(Fork::ParentOf(child_pid)) => {
            // Close write end in parent -- only child holds it now
            drop(pipe_write);
            drop(links_tx);

            CHILD_PID.store(child_pid.as_raw_nonzero().get(), Ordering::Relaxed);

            // Installed before the handlers, so a child that exits immediately
            // still finds somewhere to record it.
            let sigchld = rustix::pipe::pipe_with(
                rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
            )
            .ok();
            if let Some((_, w)) = &sigchld {
                SIGCHLD_PIPE.store(rustix::fd::AsRawFd::as_raw_fd(w), Ordering::Relaxed);
            }

            install_signal_handlers();

            let mut state =
                fs::FuseState::new(&pkg.manifest, &mut pkg.file, &pkg.footer, &pkg.codec);
            state.expect_links(links_rx);

            let mut fuse_buf = vec![0u8; 1024 * 1024 + 4096];
            state.run_loop(
                &fuse_fd,
                Some(&pipe_read),
                sigchld.as_ref().map(|(r, _)| r),
                &mut fuse_buf,
            );

            // Event loop exited -- reap child. A wake that finds the child
            // still running (the death pipe closed by an app that shuts every
            // inherited descriptor, or a stray SIGCHLD) must not park this
            // process in `waitpid`: nothing else serves the mount, so the
            // child's next read would block forever. Keep serving on the
            // exit pipe alone until the child really is gone.
            let exit_status = loop {
                match waitpid(Some(child_pid), WaitOptions::NOHANG) {
                    Ok(Some((_pid, status))) => break status,
                    Ok(None) => match &sigchld {
                        Some((r, _)) => {
                            // Drain, then look again before sleeping: an exit
                            // that landed between the check above and the
                            // drain wrote its byte into what was just thrown
                            // away, and nothing would wake the loop for it.
                            let mut scratch = [0u8; 64];
                            while rustix::io::read(r, &mut scratch).is_ok_and(|n| n > 0) {}
                            if let Ok(Some((_pid, status))) =
                                waitpid(Some(child_pid), WaitOptions::NOHANG)
                            {
                                break status;
                            }
                            state.run_loop(&fuse_fd, None::<&OwnedFd>, Some(r), &mut fuse_buf);
                        }
                        None => match waitpid(Some(child_pid), WaitOptions::empty()) {
                            Ok(Some((_pid, status))) => break status,
                            Ok(None) => continue,
                            Err(rustix::io::Errno::INTR) => continue,
                            Err(_) => {
                                cleanup_mountpoint(claim, used_namespace);
                                std::process::exit(1);
                            }
                        },
                    },
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(_) => {
                        cleanup_mountpoint(claim, used_namespace);
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
            state.stop_prefetch();
            let handed_off = match unsafe { kernel_fork() } {
                Ok(Fork::Child(_)) => {
                    detach_from_caller();
                    state.serve_detached(&fuse_fd, &mut fuse_buf, || !mount_in_use(&mountpoint));
                    drop(fuse_fd);
                    cleanup_mountpoint(claim, used_namespace);
                    std::process::exit(0);
                }
                Ok(Fork::ParentOf(_)) => true,
                // No second process to serve from, so the old behaviour is
                // all that is left: tear down and let any survivor fail.
                Err(_) => false,
            };
            if !handed_off {
                drop(fuse_fd);
                cleanup_mountpoint(claim, used_namespace);
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
            cleanup_mountpoint(claim, used_namespace);
            eprintln!("onelf-rt: fork failed: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod mount_in_use_tests {
    use super::mount_in_use;

    #[test]
    fn a_dead_mounts_directory_is_not_in_use() {
        // A forced unmount leaves the directory behind, and any process
        // still sitting in it keeps the path prefix matching. Before the
        // live-mount check that read as "in use" and serve_detached held
        // the corpse open forever.
        let dir = std::env::temp_dir().join(format!("onelf-miu-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let inside = dir.join("bin");
        std::fs::create_dir_all(&inside).unwrap();

        let _forking = crate::test_locks::forking();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .current_dir(&inside)
            .spawn()
            .expect("spawn holder");

        assert!(!mount_in_use(&dir), "a non-mount directory has no users");
        assert!(
            super::tree_in_use(&dir),
            "the directory itself still has a user"
        );

        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A process run under a loader names the loader as its executable
    /// and may sit anywhere, so only its maps show it running from the
    /// tree. A daemon started that way lost its mount a second after the
    /// foreground exited.
    #[test]
    fn a_process_mapping_a_file_from_the_tree_is_a_user() {
        // A shell rather than sleep: uutils coreutils refuses to run when
        // its executable is the loader rather than itself.
        let program = std::path::Path::new("/bin/sh");
        let Some(loader) = crate::interp::read_elf_interp(program) else {
            return; // a static shell has no loader to be run under
        };
        let dir = std::env::temp_dir().join(format!("onelf-maps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let copy = dir.join("sh");
        std::fs::copy(program, &copy).unwrap();

        let _forking = crate::test_locks::forking();
        let mut child = std::process::Command::new(&loader)
            .arg(&copy)
            .args(["-c", "read line"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("spawn under the loader");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut seen = false;
        while !seen && std::time::Instant::now() < deadline {
            seen = super::tree_in_use(&dir);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(seen, "{loader} running {} uses the tree", copy.display());
    }
}

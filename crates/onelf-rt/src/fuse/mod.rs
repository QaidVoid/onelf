//! FUSE-based execution mode.
//!
//! Mounts the package contents as a read-only FUSE filesystem and executes
//! the entrypoint directly from the mount. Concurrent launches of one
//! package share a mount and the server behind it (see [`share`]). A
//! launch that does not share forks its own server, which is the launch's
//! guardian (see [`crate::guardian`]): it serves the mount while the app
//! runs and until `wait` proves every descendant gone.

pub mod fs;
pub(crate) use fs::TRACE_ARM;
pub mod mount;
mod protocol;
pub(crate) mod share;

use std::ffi::OsString;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};

use rustix::io::FdFlags;
use rustix::process::Signal;
use rustix::runtime::{KernelSigSet, KernelSigaction, KernelSigactionFlags, kernel_sigaction};

use crate::loader::PackageData;

/// Write end of the pipe the `SIGCHLD` handler writes a byte to, so the
/// private server's event loop wakes when a child of its subtree exits:
/// `poll` cannot wait on a signal, so the signal has to become readable
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
// calls sigreturn (not rt_sigreturn). A handler installed with SA_SIGINFO
// gets the rt frame instead, which returns through rt_sigreturn.
#[cfg(target_arch = "x86")]
core::arch::global_asm!(
    ".global __onelf_signal_restorer",
    ".type __onelf_signal_restorer, @function",
    "__onelf_signal_restorer:",
    "pop eax",      // discard the signal number pushed on the legacy frame
    "mov eax, 119", // __NR_sigreturn
    "int 0x80",
    ".global __onelf_rt_signal_restorer",
    ".type __onelf_rt_signal_restorer, @function",
    "__onelf_rt_signal_restorer:",
    "mov eax, 173", // __NR_rt_sigreturn
    "int 0x80",
);

unsafe extern "C" {
    fn __onelf_signal_restorer();
}

/// Wake the event loop: one byte on [`SIGCHLD_PIPE`]. Writing a byte is
/// the whole handler, which keeps it async-signal-safe.
unsafe extern "C" fn wake_on_child_exit(_sig: core::ffi::c_int) {
    let fd = SIGCHLD_PIPE.load(Ordering::Relaxed);
    if fd >= 0 {
        let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
        let _ = rustix::io::write(borrowed, b"c");
    }
}

/// Make `SIGCHLD` write to `pipe`, so the server loop can wait on the
/// subtree as it serves. Only an exit is worth waking for: without
/// NOCLDSTOP a Ctrl-Z, a `kill -STOP`, or a debugger attaching would also
/// raise it, and the loop would reap nothing.
fn wake_on_child_exits(pipe: &OwnedFd) {
    SIGCHLD_PIPE.store(rustix::fd::AsRawFd::as_raw_fd(pipe), Ordering::Relaxed);
    let action = KernelSigaction {
        sa_handler_kernel: Some(wake_on_child_exit),
        sa_flags: KernelSigactionFlags::RESTORER | KernelSigactionFlags::NOCLDSTOP,
        sa_restorer: Some(__onelf_signal_restorer),
        sa_mask: KernelSigSet::empty(),
    };
    unsafe {
        let _ = kernel_sigaction(Signal::CHILD, Some(action));
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
    /// `resolved` hands in a decision the caller already made, or `None`
    /// to resolve here.
    fn launch<'b>(
        &'b self,
        pkg: &'b PackageData,
        pkg_root: &'b Path,
        private_ns: bool,
        links: crate::launch::Links<'b>,
        resolved: Option<crate::launch::Resolved>,
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
            resolved,
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
    crate::guardian::detach_stdio();
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
            eprintln!("onelf-rt: fuse: fusermount3 not available either; FUSE is unavailable");
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

/// Mount the package for this launch alone and run the app from it.
///
/// The server is the launch's guardian: this process forks it once the
/// mount exists and stays behind as the proxy, and the server marks
/// itself a child subreaper and forks the app. It serves the mount while
/// it reaps its subtree, reports the app's status the moment the app has
/// been reaped, and takes the mount down only when `wait` proves every
/// descendant gone, so a daemon the app left behind, whatever it closed,
/// keeps its files. No process started after the app exits ever stands
/// in for it.
fn execute_private(pkg: &mut PackageData, req: &Request, needs_setuid: bool) -> bool {
    // Held by the server for as long as it serves, so no concurrent sweep
    // can reclaim the directory.
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
    // Set CLOEXEC so the app doesn't inherit the FUSE fd after exec.
    let _ = rustix::io::fcntl_setfd(&fuse_fd, FdFlags::CLOEXEC);

    // Set up portable directories (doesn't access FUSE mount)
    req.setup_portable();

    // The app resolves which libraries the mount shows from outside the
    // package, since only it reads the mount, and the server serves them
    // together with the app's generated environment input.
    let channel = crate::links::channel();
    let wake = rustix::pipe::pipe_with(
        rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
    );
    let (Ok((links_tx, links_rx)), Ok((wake_r, wake_w))) = (channel, wake) else {
        cleanup_mountpoint(claim, used_namespace);
        return false;
    };

    let server = Private {
        fuse_fd,
        claim,
        used_namespace,
        links_tx,
        links_rx,
        wake_r,
        wake_w,
    };
    match crate::guardian::fork_guardian(|report| serve_private(pkg, req, server, report)) {
        // This process held copies of the mount's descriptors only until
        // the fork, so a server that dies takes the connection with it.
        Ok(supervision) => crate::guardian::proxy(supervision, "fuse"),
        Err(e) => {
            eprintln!("onelf-rt: fuse: fork failed: {e}");
            std::process::exit(1);
        }
    }
}

/// What the proxy hands the private server it forks.
struct Private {
    fuse_fd: OwnedFd,
    claim: crate::paths::Mountpoint,
    used_namespace: bool,
    links_tx: crate::links::Sender,
    links_rx: crate::links::Receiver,
    wake_r: OwnedFd,
    wake_w: OwnedFd,
}

/// The private server's side of one launch: the guardian of the app, and
/// the server of its mount. Never returns.
fn serve_private(
    pkg: &mut PackageData,
    req: &Request,
    server: Private,
    report: crate::guardian::Report,
) -> ! {
    let Private {
        fuse_fd,
        claim,
        used_namespace,
        links_tx,
        links_rx,
        wake_r,
        wake_w,
    } = server;
    let mountpoint = claim.path().to_path_buf();

    // Where the kernel refuses the attribute, orphans go elsewhere and
    // `wait` cannot speak for them; the server then also asks `/proc`
    // before it lets the mount go, as it always has.
    let subreaper = match crate::guardian::mark_subreaper() {
        Ok(()) => true,
        Err(e) => {
            eprintln!("onelf-rt: fuse: child subreaper unavailable: {e}");
            false
        }
    };
    crate::guardian::prepare_reaping();
    crate::guardian::install_forwarding();
    wake_on_child_exits(&wake_w);

    let mut inbox = Some(links_rx);
    let app = {
        let inbox = &mut inbox;
        let pkg: &PackageData = pkg;
        let mountpoint = mountpoint.clone();
        let connection = std::os::fd::AsRawFd::as_raw_fd(&fuse_fd);
        crate::guardian::fork_foreground(move || {
            // The connection is the server's alone. A copy held here until
            // the exec would keep a killed server's mount alive, and a read
            // of the mount this child was in would then wait forever,
            // unkillably, for an answer only the dead server could give.
            // SAFETY: this forked child never touches the server's handle
            // again; it execs or exits.
            unsafe { rustix::io::close(connection) };
            // The server's ends of the hand-off are not the app's: an ack
            // pipe the app held would never hang up on a dead server.
            drop(inbox.take());
            // Everything that reads the mount has to happen in the app,
            // after the fork: this server's event loop is what serves it,
            // and that loop is only now starting.
            crate::launch::exec(&req.launch(
                pkg,
                &mountpoint,
                used_namespace,
                crate::launch::Links::Server(&links_tx),
                None,
            ))
        })
    };
    let app = match app {
        Ok(app) => app,
        Err(e) => {
            eprintln!("onelf-rt: fuse: fork failed: {e}");
            report.send(crate::guardian::Outcome::Unavailable);
            cleanup_mountpoint(claim, used_namespace);
            std::process::exit(1);
        }
    };

    let mut state = fs::FuseState::new(&pkg.manifest, &mut pkg.file, &pkg.footer, &pkg.codec);
    if let Some(inbox) = inbox {
        state.expect_links(inbox);
    }
    let mut report = Some(report);
    let mut buf = vec![0u8; 1024 * 1024 + 4096];
    loop {
        let end = state.run_loop(&fuse_fd, None::<&OwnedFd>, Some(&wake_r), &mut buf);
        let mut scratch = [0u8; 64];
        while rustix::io::read(&wake_r, &mut scratch).is_ok_and(|n| n > 0) {}
        if crate::guardian::reap_exited(app, &mut report) {
            break;
        }
        if end == fs::LoopEnd::Finished {
            // The connection is gone, so nothing is left to serve; the
            // subtree is still reaped to the end.
            crate::guardian::reap_all(app, &mut report);
            break;
        }
    }
    if !subreaper {
        state.serve_detached(&fuse_fd, &mut buf, || !mount_in_use(&mountpoint));
    }
    drop(fuse_fd);
    cleanup_mountpoint(claim, used_namespace);
    std::process::exit(0);
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

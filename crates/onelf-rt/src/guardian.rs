//! The per-launch lifetime guardian.
//!
//! A filesystem-backed launch can outlive its caller: an application that
//! daemonizes leaves descendants running from the tree while the invoked
//! process is long gone. Inherited lease descriptors cannot carry the
//! launch through that, because ordinary daemonization and a wholesale
//! `close_range` discard them, and a supervisor forked after the
//! application's exit cannot become the ancestor the orphans re-parent to.
//! So the runtime forks the guardian first: it takes the launch's claims,
//! marks itself a child subreaper, and forks the application from there.
//!
//! The guardian reports the application's exit status to the invoking
//! process the moment it has been reaped, so the caller returns promptly
//! while background descendants keep running. It forwards each termination
//! signal it receives to the foreground child exactly once. It adopts
//! every descendant orphaned by a double fork, reaps each one so none is
//! left a zombie, and releases the launch's claims only after a drained
//! `wait` proves its subtree empty. No process appearing after the application's
//! exit can take the ancestry from it: the orphans' nearest subreaper
//! ancestor was fixed before the application ever forked.

use std::io;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicI32, Ordering};

use core::ffi::c_int;

use rustix::io::Errno;
use rustix::pipe::PipeFlags;
use rustix::process::{
    Pid, Signal, WaitOptions, WaitStatus, child_subreaper, kill_process, set_child_subreaper, wait,
    waitpid,
};
use rustix::runtime::{
    Fork, KernelSigSet, KernelSigaction, KernelSigactionFlags, kernel_fork, kernel_sigaction,
};

// Signal restorer: kernel_sigaction bypasses libc, and x86_64 and i386
// Linux need one for signal return to work. Defined once for the runtime
// binary in `fuse`; this declaration links to that symbol.
unsafe extern "C" {
    fn __onelf_signal_restorer();
}

/// The process id the forwarding handlers aim at: the foreground child
/// while it lives, zero once it has been reaped. Zero keeps a recycled
/// pid from receiving a stale forward.
static FOREGROUND: AtomicI32 = AtomicI32::new(0);

/// How the foreground process ended, as the guardian reports it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// Exited with this status.
    Exit(i32),
    /// Was terminated by this signal.
    Signaled(i32),
    /// Ended in a way with neither an exit status nor a signal.
    Other,
    /// The guardian could not put the lifetime guarantees in place, before
    /// or while the launch ran.
    Unavailable,
}

impl Outcome {
    /// The exit code the invoking process exits with, the conversion the
    /// launch modes have always applied to a child status.
    pub fn exit_code(self) -> i32 {
        match self {
            Outcome::Exit(code) => code,
            Outcome::Signaled(sig) => 128 + sig,
            Outcome::Other | Outcome::Unavailable => 1,
        }
    }
}

/// The record reporting [`Outcome::Unavailable`]: written when no launch
/// ran to report a status for.
const UNAVAILABLE_RECORD: [u8; 2] = [3, 0];

/// The record on the report pipe: a kind byte, then a value byte.
fn encode_record(outcome: Outcome) -> [u8; 2] {
    match outcome {
        Outcome::Exit(code) => [0, code as u8],
        Outcome::Signaled(sig) => [1, sig as u8],
        Outcome::Other => [2, 0],
        Outcome::Unavailable => UNAVAILABLE_RECORD,
    }
}

/// [`encode_record`] read back.
fn decode_record(record: [u8; 2]) -> Outcome {
    match record[0] {
        0 => Outcome::Exit(i32::from(record[1])),
        1 => Outcome::Signaled(i32::from(record[1])),
        3 => Outcome::Unavailable,
        _ => Outcome::Other,
    }
}

/// The foreground child's wait status as an [`Outcome`].
fn outcome(status: WaitStatus) -> Outcome {
    if let Some(code) = status.exit_status() {
        Outcome::Exit(code)
    } else if let Some(sig) = status.terminating_signal() {
        Outcome::Signaled(sig)
    } else {
        Outcome::Other
    }
}

/// What the invoking process keeps for one supervised launch.
pub struct Supervision {
    /// Read end of the pipe the guardian reports on. One two-byte record
    /// arrives the moment the foreground process has been reaped; a hang
    /// up without one means the guardian died without reporting.
    pub status: OwnedFd,
    /// The guardian's process id. Signals aimed at it reach the foreground
    /// child, forwarded once each, while the child lives.
    pub pid: u32,
}

/// Fork the guardian for one launch and, inside it, the foreground child.
///
/// The guardian takes `lease` (the launch's claims) with it and holds the
/// descriptors itself for the whole supervised lifetime, so a descendant
/// that closes or clears everything it inherited cannot end the claims.
/// It marks itself a child subreaper before forking the foreground child,
/// which runs `start` and is expected to `exec` and never return. As soon
/// as the child has been reaped, the guardian leaves the caller's session,
/// lets go of the caller's standard streams, and reports the child's
/// [`Outcome`] on [`Supervision::status`]. It then adopts and reaps every
/// orphaned descendant, runs `release(lease)` once a drained `wait` proves
/// the subtree empty, and exits.
///
/// This returns in the invoking process only. Launching fails with an
/// error when the report pipe or a fork fails, and the [`Outcome::Unavailable`]
/// record says so when the kernel refuses the subreaper attribute.
pub fn spawn<L, F, R>(lease: L, start: F, release: R) -> io::Result<Supervision>
where
    F: FnOnce(),
    R: FnOnce(L),
{
    let (report_read, report_write) =
        rustix::pipe::pipe_with(PipeFlags::CLOEXEC).map_err(io::Error::from)?;

    match unsafe { kernel_fork() } {
        Ok(Fork::Child(_)) => {
            drop(report_read);
            supervise(lease, report_write, start, release);
        }
        Ok(Fork::ParentOf(child)) => {
            drop(report_write);
            Ok(Supervision {
                status: report_read,
                pid: child.as_raw_nonzero().get() as u32,
            })
        }
        Err(e) => Err(io::Error::from(e)),
    }
}

/// The guardian's side of one launch. Never returns: the process exits.
fn supervise<L, F, R>(lease: L, report: OwnedFd, start: F, release: R) -> !
where
    F: FnOnce(),
    R: FnOnce(L),
{
    // The attribute has to be in place before the fork: a descendant that
    // double forks at once would otherwise already be beyond reach. With
    // no descendant forked yet, a failed attribute releases the launch at
    // once and says so.
    if mark_subreaper().is_err() {
        release(lease);
        write_record(&report, UNAVAILABLE_RECORD);
        std::process::exit(1);
    }
    install_forwarding();

    let child = match unsafe { kernel_fork() } {
        Ok(Fork::Child(_)) => {
            start();
            // A `start` that returns ran nothing that reports a status.
            std::process::exit(0);
        }
        Ok(Fork::ParentOf(child)) => child,
        Err(e) => {
            eprintln!("onelf-rt: guardian: fork failed: {e}");
            release(lease);
            write_record(&report, UNAVAILABLE_RECORD);
            std::process::exit(1);
        }
    };
    FOREGROUND.store(child.as_raw_nonzero().get(), Ordering::Relaxed);

    let status = loop {
        match waitpid(Some(child), WaitOptions::empty()) {
            Ok(Some((_, status))) => break status,
            Ok(None) | Err(Errno::INTR) => continue,
            Err(e) => {
                eprintln!("onelf-rt: guardian: wait failed: {e}");
                // The foreground is unaccounted for, so nothing proves the
                // launch idle and the claims go with the sweep grace
                // instead of being released here.
                write_record(&report, UNAVAILABLE_RECORD);
                std::process::exit(1);
            }
        }
    };
    FOREGROUND.store(0, Ordering::Relaxed);

    // Detached before the caller learns the foreground is gone, so the
    // session the caller leaves cannot take the guardian down before the
    // descendants have been reaped.
    detach_stdio();

    write_record(&report, encode_record(outcome(status)));
    drop(report);

    // Every orphan the kernel hands over is reaped here exactly once, and
    // the loop ends only when `wait` says the subtree is empty, which is
    // the proof the release waits for. The plain `wait` is load-bearing:
    // a wait on the caller's process group would follow a daemonized
    // descendant only until its first `setsid`, and report an empty
    // subtree while the launch still runs. Anything but an interrupt or
    // the empty-subtree error cannot happen to a blocking wait, and
    // releasing then beats waiting on an unknown error forever.
    loop {
        match wait(WaitOptions::empty()) {
            Ok(Some(_)) => continue,
            Ok(None) | Err(Errno::INTR) => continue,
            Err(Errno::CHILD) | Err(_) => break,
        }
    }

    release(lease);
    std::process::exit(0);
}

/// Mark this process a child subreaper, so descendants orphaned by a
/// double fork re-parent here instead of to init, and check the kernel
/// kept the attribute.
pub(crate) fn mark_subreaper() -> io::Result<()> {
    // PR_SET_CHILD_SUBREAPER reads the argument only as set or unset, and
    // answers 1 for enabled.
    set_child_subreaper(Pid::from_raw(1))?;
    if child_subreaper()?.is_none() {
        return Err(io::Error::other("child subreaper attribute did not hold"));
    }
    Ok(())
}

/// Forward the caller-facing termination signals to the foreground child.
fn install_forwarding() {
    let mut mask = KernelSigSet::empty();
    for sig in [Signal::INT, Signal::TERM, Signal::HUP, Signal::QUIT] {
        mask.insert(sig);
    }
    for sig in [Signal::INT, Signal::TERM, Signal::HUP, Signal::QUIT] {
        let action = KernelSigaction {
            sa_handler_kernel: Some(forward_to_foreground),
            sa_flags: KernelSigactionFlags::RESTORER,
            sa_restorer: Some(__onelf_signal_restorer),
            sa_mask: mask.clone(),
        };
        unsafe {
            let _ = kernel_sigaction(sig, Some(action));
        }
    }
}

/// One copy of the signal to the foreground child, or nothing when no
/// child is being waited for. Only async-signal-safe calls.
unsafe extern "C" fn forward_to_foreground(sig: c_int) {
    if let Some(pid) = Pid::from_raw(FOREGROUND.load(Ordering::Relaxed))
        && let Some(sig) = Signal::from_named_raw(sig)
    {
        let _ = kill_process(pid, sig);
    }
}

/// Leave the caller's session and let go of its standard streams.
///
/// A launch that outlives its caller must not die with the caller's
/// session: a Ctrl-C aimed at the caller's foreground process group, or
/// the hangup that follows a session leader's exit, would otherwise take
/// down the process still guarding the tree. Holding the streams also
/// keeps the write end of whatever pipe the caller set up open, so `$(app)`
/// and `app | head` would block until the guardian exits rather than until
/// the application is done; the streams have to go.
pub(crate) fn detach_stdio() {
    let _ = rustix::process::setsid();
    if let Ok(null) = rustix::fs::open(
        "/dev/null",
        rustix::fs::OFlags::RDWR,
        rustix::fs::Mode::empty(),
    ) {
        for target in 0..=2 {
            // `dup2` wants to own the descriptor it replaces, but 0, 1 and
            // 2 are not ours to close: the dup is what puts /dev/null
            // there, and dropping the wrapper afterwards would close it
            // again.
            use std::os::fd::FromRawFd;
            let mut slot =
                std::mem::ManuallyDrop::new(unsafe { std::os::fd::OwnedFd::from_raw_fd(target) });
            let _ = rustix::io::dup2(&null, &mut slot);
        }
    }
}

/// Write the whole record, retrying what a signal interrupts and what the
/// pipe accepts in pieces. A failed write stays a failed report: the
/// guardian's later exit hangs the pipe up, and the caller reads that as
/// a missing report.
fn write_record(report: &OwnedFd, record: [u8; 2]) {
    let mut written = 0;
    while written < record.len() {
        match rustix::io::write(report, &record[written..]) {
            Ok(n) => written += n,
            Err(Errno::INTR) => continue,
            Err(_) => return,
        }
    }
}

/// Read the foreground [`Outcome`] the guardian reports, waiting until the
/// record arrives or the pipe hangs up. A hang up without a record is an
/// error: the guardian died without reporting.
pub fn receive(status: &OwnedFd) -> io::Result<Outcome> {
    let mut record = [0u8; 2];
    let mut filled = 0;
    while filled < record.len() {
        match rustix::io::read(status, &mut record[filled..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the guardian exited without reporting a status",
                ));
            }
            Ok(n) => filled += n,
            Err(Errno::INTR) => continue,
            Err(e) => return Err(io::Error::from(e)),
        }
    }
    Ok(decode_record(record))
}

#[cfg(test)]
mod tests {
    use core::ffi::c_int;
    use std::io::Read;
    use std::os::fd::{BorrowedFd, IntoRawFd};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use rustix::fs::{FlockOperation, Mode, OFlags};
    use rustix::process::{
        Pid, Signal, child_subreaper, getpid, kill_process, set_child_subreaper,
    };
    use rustix::runtime::{
        Fork, KernelSigSet, KernelSigaction, KernelSigactionFlags, kernel_fork, kernel_sigaction,
    };

    use super::{Outcome, decode_record, encode_record};

    // The supervisor process of one launch stands in for the guardian in
    // these tests, the way the runtime drives it: fork the machinery in a
    // quiet process, read the report, and follow the release from files.
    //
    // The drivers run the workers as `#[ignore]` tests of this same
    // binary, each alone in a process of its own. A fork of the
    // multi-threaded test harness can leave the forked process a malloc
    // lock held by another test's thread, which deadlocks the first
    // allocation it makes; a worker process runs its one test on a quiet
    // harness, so the raw forks there are as safe as the launch's own.
    fn worker_dir() -> PathBuf {
        let dir = std::env::var_os("ONELF_GUARDIAN_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| panic!("worker started without ONELF_GUARDIAN_DIR"));
        std::fs::create_dir_all(&dir).expect("create the scratch directory");
        dir
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("onelf-guardian-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the scratch directory");
        dir
    }

    /// Whether this kernel takes the child subreaper attribute at all.
    /// Sets and clears it on the caller, so a `true` answer leaves nothing
    /// behind.
    fn subreaper_supported() -> bool {
        if super::mark_subreaper().is_err() {
            return false;
        }
        let _ = set_child_subreaper(None);
        true
    }

    /// A stand-in for the launch's claims: a shared lock on its own file,
    /// inheritable like the real ones, probed from the driver by trying
    /// the exclusive lock on the path.
    fn lease_lock(dir: &Path) -> std::fs::File {
        let file =
            crate::paths::open_lock_inheritable(&dir.join("lease.lock")).expect("lease file");
        rustix::fs::flock(&file, FlockOperation::LockShared).expect("flock the lease");
        file
    }

    /// Whether the lease on `path` is free: an exclusive nonblocking lock
    /// succeeds only when no guardian holds it shared.
    fn lease_is_free(path: &Path) -> bool {
        let Ok(file) = std::fs::File::open(path) else {
            return true;
        };
        rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive).is_ok()
    }

    fn ppid_of(pid: u32) -> Option<u32> {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        status.lines().find_map(|line| {
            line.strip_prefix("PPid:")
                .and_then(|rest| rest.trim().parse().ok())
        })
    }

    fn alive(pid: u32) -> bool {
        ppid_of(pid).is_some()
    }

    fn read_u32_file(path: &Path) -> Option<u32> {
        std::fs::read_to_string(path).ok()?.trim().parse().ok()
    }

    /// Bounded poll for a file to appear. The workers and drivers wait on
    /// files rather than pipes so neither side can block the other for
    /// good.
    fn wait_for(path: &Path, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if path.exists() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Bounded poll for a process to be gone from `/proc`, so a reaped
    /// descendant is told apart from one left a zombie.
    fn gone(pid: u32, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if !alive(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Run one `#[ignore]` worker of this binary, alone in a process of
    /// its own. The driver holds the fork lock for the worker's whole run,
    /// as a test that starts a process must.
    fn run_worker(worker: &str, dir: &Path) {
        let _forking = crate::test_locks::forking();
        let exe = std::env::current_exe().expect("the test binary's path");
        let mut child = std::process::Command::new(exe)
            .arg(format!("guardian::tests::{worker}"))
            .args(["--exact", "--ignored"])
            .env("ONELF_GUARDIAN_DIR", dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn the worker process");
        let deadline = Instant::now() + Duration::from_secs(120);
        let status = loop {
            match child.try_wait().expect("poll the worker") {
                Some(status) => break status,
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("worker {worker} did not finish in 120s");
                }
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        };
        let mut out = String::new();
        let mut err = String::new();
        if let Some(mut pipe) = child.stdout.take() {
            let _ = pipe.read_to_string(&mut out);
        }
        if let Some(mut pipe) = child.stderr.take() {
            let _ = pipe.read_to_string(&mut err);
        }
        assert!(
            status.success(),
            "worker {worker} failed: {status}\n{out}\n{err}"
        );
    }

    /// Fork the daemon the way a daemonizing application does: a child
    /// that forks again and leaves the grandchild behind. The grandchild
    /// is its own session, lets go of the caller's streams the way a
    /// daemon does, puts its pid on file, and leads a bounded life.
    fn fork_daemon(dir: &Path, sleep_secs: u64, exit_code: i32) {
        match unsafe { kernel_fork() } {
            Ok(Fork::Child(_)) => match unsafe { kernel_fork() } {
                Ok(Fork::Child(_)) => {
                    super::detach_stdio();
                    let _ = std::fs::write(dir.join("daemon_pid"), std::process::id().to_string());
                    std::thread::sleep(Duration::from_secs(sleep_secs));
                    std::process::exit(exit_code);
                }
                Ok(Fork::ParentOf(_)) => std::process::exit(0),
                Err(_) => std::process::exit(1),
            },
            Ok(Fork::ParentOf(_)) => {}
            Err(_) => std::process::exit(1),
        }
    }

    /// The driver of the double fork: ancestry, the original status, and
    /// both timings.
    #[test]
    fn double_forked_daemon_reparents_to_the_guardian() {
        let dir = temp_dir("double-fork");
        run_worker("worker_double_forked_daemon_reparents", &dir);
        if dir.join("unsupported").exists() {
            eprintln!("skipping: this kernel does not take the child subreaper attribute");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let guardian = read_u32_file(&dir.join("guardian_pid")).expect("guardian pid file");
        let daemon = read_u32_file(&dir.join("daemon_pid")).expect("daemon pid file");

        // The caller is long gone, the daemon sleeps on: the guardian must
        // still be there, holding the launch.
        assert!(alive(guardian), "the guardian outlives the caller");
        assert!(alive(daemon), "the daemon still runs");
        assert!(
            !dir.join("released").exists(),
            "nothing is released while a descendant lives"
        );
        assert!(
            !lease_is_free(&dir.join("lease.lock")),
            "the guardian still holds the lease"
        );

        // The daemon leaves on its own; then the release, then the exit,
        // with nothing left a zombie.
        assert!(
            wait_for(&dir.join("released"), 15),
            "the guardian never released the launch"
        );
        assert!(
            gone(guardian, 15),
            "the guardian never exited after its subtree emptied"
        );
        assert!(gone(daemon, 5), "the re-parented daemon was left a zombie");
        assert!(lease_is_free(&dir.join("lease.lock")), "the lease ended");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Runs in its own process: supervises a foreground that daemonizes
    /// with a double fork, and checks the report and the ancestry.
    #[test]
    #[ignore = "driven by double_forked_daemon_reparents_to_the_guardian, in a quiet process"]
    fn worker_double_forked_daemon_reparents() {
        let dir = worker_dir();
        if !subreaper_supported() {
            let _ = std::fs::write(dir.join("unsupported"), b"");
            return;
        }
        let lease = lease_lock(&dir);
        let release = {
            let dir = dir.clone();
            move |_lease: std::fs::File| {
                let _ = std::fs::write(dir.join("released"), b"1");
            }
        };
        let start = {
            let dir = dir.clone();
            move || {
                fork_daemon(&dir, 5, 9);
                std::process::exit(7);
            }
        };
        let supervision = super::spawn(lease, start, release).expect("spawn the guardian");

        let began = Instant::now();
        let outcome = super::receive(&supervision.status).expect("the reported status");
        assert_eq!(outcome, Outcome::Exit(7), "the original foreground status");
        assert!(
            began.elapsed() < Duration::from_secs(4),
            "the status took {:?}; the daemon only sleeps 5s",
            began.elapsed()
        );

        assert!(
            wait_for(&dir.join("daemon_pid"), 3),
            "the daemon never reported its pid"
        );
        let daemon = read_u32_file(&dir.join("daemon_pid")).expect("daemon pid file");
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut adopted = false;
        while Instant::now() < deadline {
            match ppid_of(daemon) {
                Some(ppid) if ppid == supervision.pid => {
                    adopted = true;
                    break;
                }
                Some(1) => panic!("the daemon re-parented to init, not the guardian"),
                _ => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        assert!(
            adopted,
            "the guardian is the double forked daemon's ancestor"
        );
        assert!(
            !dir.join("released").exists(),
            "the guardian holds the launch while the daemon lives"
        );
        let _ = std::fs::write(dir.join("guardian_pid"), supervision.pid.to_string());
    }

    /// The driver of the lease against `close_range`: a descendant that
    /// closes everything it inherited cannot end the lease the guardian
    /// holds itself.
    #[test]
    fn close_range_cannot_end_the_lease_the_guardian_holds() {
        let dir = temp_dir("close-range");
        run_worker("worker_close_range_lease", &dir);
        if dir.join("unsupported").exists() {
            eprintln!("skipping: this kernel does not take the child subreaper attribute");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let daemon = wait_for(&dir.join("daemon_pid"), 5)
            .then(|| read_u32_file(&dir.join("daemon_pid")))
            .flatten()
            .expect("daemon pid file");

        // The descendant has closed every descriptor it inherited, and it
        // lives on: the lease must survive on the guardian's own hold.
        assert!(
            wait_for(&dir.join("closed"), 5),
            "the descendant never got past closing its descriptors"
        );
        assert!(alive(daemon), "the descendant that closed everything lives");
        assert!(
            !lease_is_free(&dir.join("lease.lock")),
            "the lease survived the descendant's close_range"
        );

        assert!(
            wait_for(&dir.join("released"), 15),
            "the guardian never released the launch"
        );
        let guardian = read_u32_file(&dir.join("guardian_pid")).expect("guardian pid file");
        assert!(gone(guardian, 15), "the guardian never exited");
        assert!(gone(daemon, 5), "the descendant was left a zombie");
        assert!(lease_is_free(&dir.join("lease.lock")), "the lease ended");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Runs in its own process: the foreground reports its status as
    /// killed by a signal while a descendant that closed everything keeps
    /// the lease alive.
    #[test]
    #[ignore = "driven by close_range_cannot_end_the_lease_the_guardian_holds, in a quiet process"]
    fn worker_close_range_lease() {
        let dir = worker_dir();
        if !subreaper_supported() {
            let _ = std::fs::write(dir.join("unsupported"), b"");
            return;
        }
        let lease = lease_lock(&dir);
        let release = {
            let dir = dir.clone();
            move |_lease: std::fs::File| {
                let _ = std::fs::write(dir.join("released"), b"1");
            }
        };
        let start = {
            let dir = dir.clone();
            move || {
                fork_daemon_closing_all(&dir, 5, 9);
                // The original invocation ends killed by a signal of its
                // own: the status the caller reads is a signal death even
                // though a descendant lives on.
                let _ = kill_process(getpid(), Signal::USR1);
                std::process::exit(1);
            }
        };
        let supervision = super::spawn(lease, start, release).expect("spawn the guardian");

        let began = Instant::now();
        let outcome = super::receive(&supervision.status).expect("the reported status");
        assert_eq!(
            outcome,
            Outcome::Signaled(10),
            "the foreground's death by signal, reported as it happened"
        );
        assert_eq!(outcome.exit_code(), 138);
        assert!(
            began.elapsed() < Duration::from_secs(4),
            "the status took {:?}; the descendant only sleeps 5s",
            began.elapsed()
        );
        assert!(
            !dir.join("released").exists(),
            "the guardian holds the launch while the descendant lives"
        );
        let _ = std::fs::write(dir.join("guardian_pid"), supervision.pid.to_string());
    }

    /// The daemon of the close-range case: after `setsid` it discards
    /// every descriptor it inherited, the way `close_range(0, ~0)` would,
    /// and proves it is alive past that with a marker file.
    fn fork_daemon_closing_all(dir: &Path, sleep_secs: u64, exit_code: i32) {
        match unsafe { kernel_fork() } {
            Ok(Fork::Child(_)) => {
                let _ = rustix::process::setsid();
                let _ = std::fs::write(dir.join("daemon_pid"), format!("{}", std::process::id()));
                // Rustix's `close` treats an already closed descriptor as
                // a bug, so the wholesale wipe asks `fcntl` first.
                for fd in 0..4096 {
                    if rustix::io::fcntl_getfd(unsafe { BorrowedFd::borrow_raw(fd) }).is_ok() {
                        unsafe { rustix::io::close(fd) };
                    }
                }
                let _ = std::fs::write(dir.join("closed"), b"1");
                std::thread::sleep(Duration::from_secs(sleep_secs));
                std::process::exit(exit_code);
            }
            Ok(Fork::ParentOf(_)) => {}
            Err(_) => std::process::exit(1),
        }
    }

    /// The file descriptors the counting handler writes its byte to, one
    /// per signal. Set before the handlers are armed, and never touched
    /// again, so the handler stays async-signal-safe.
    static TERM_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
    static INT_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

    /// Record one received signal as a single byte. Only
    /// async-signal-safe calls.
    unsafe extern "C" fn count_signal(sig: c_int) {
        let fd = match sig {
            15 => &TERM_FD,
            2 => &INT_FD,
            _ => return,
        };
        let raw = fd.load(std::sync::atomic::Ordering::Relaxed);
        if raw >= 0 {
            let borrowed = unsafe { BorrowedFd::borrow_raw(raw) };
            let _ = rustix::io::write(borrowed, b"x");
        }
    }

    fn install_counter(sig: Signal, fd: std::os::fd::OwnedFd) {
        let slot = match sig {
            Signal::TERM => &TERM_FD,
            Signal::INT => &INT_FD,
            _ => unreachable!("only TERM and INT are counted"),
        };
        slot.store(fd.into_raw_fd(), std::sync::atomic::Ordering::Relaxed);
        let action = KernelSigaction {
            sa_handler_kernel: Some(count_signal),
            sa_flags: KernelSigactionFlags::RESTORER,
            sa_restorer: Some(super::__onelf_signal_restorer),
            sa_mask: KernelSigSet::empty(),
        };
        unsafe {
            kernel_sigaction(sig, Some(action)).expect("arm the counting handler");
        }
    }

    /// The driver of signal forwarding: one TERM and one INT aimed at the
    /// guardian reach the foreground child once each.
    #[test]
    fn signals_reach_the_foreground_child_once_each() {
        let dir = temp_dir("signals");
        run_worker("worker_signal_forwarding", &dir);
        if dir.join("unsupported").exists() {
            eprintln!("skipping: this kernel does not take the child subreaper attribute");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        assert!(
            wait_for(&dir.join("released"), 15),
            "the guardian never released the launch"
        );
        let guardian = read_u32_file(&dir.join("guardian_pid")).expect("guardian pid file");
        assert!(gone(guardian, 15), "the guardian never exited");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Runs in its own process: sends one TERM and one INT to the
    /// guardian, and checks the child counted exactly one of each and
    /// lived on to exit with its own status.
    #[test]
    #[ignore = "driven by signals_reach_the_foreground_child_once_each, in a quiet process"]
    fn worker_signal_forwarding() {
        let dir = worker_dir();
        if !subreaper_supported() {
            let _ = std::fs::write(dir.join("unsupported"), b"");
            return;
        }
        let lease = lease_lock(&dir);
        let release = {
            let dir = dir.clone();
            move |_lease: std::fs::File| {
                let _ = std::fs::write(dir.join("released"), b"1");
            }
        };
        let start = {
            let dir = dir.clone();
            move || {
                let terms = open_append(&dir.join("terms"));
                let ints = open_append(&dir.join("ints"));
                install_counter(Signal::TERM, terms);
                install_counter(Signal::INT, ints);
                let _ = std::fs::write(dir.join("armed"), b"1");
                std::thread::sleep(Duration::from_secs(6));
                std::process::exit(11);
            }
        };
        let supervision = super::spawn(lease, start, release).expect("spawn the guardian");
        assert!(
            wait_for(&dir.join("armed"), 5),
            "the child never armed its counting handlers"
        );

        let guardian = Pid::from_raw(supervision.pid as i32).expect("the guardian's pid");
        kill_process(guardian, Signal::TERM).expect("signal the guardian");
        kill_process(guardian, Signal::INT).expect("signal the guardian");

        let outcome = super::receive(&supervision.status).expect("the reported status");
        assert_eq!(
            outcome,
            Outcome::Exit(11),
            "the child survived one TERM and one INT and exited on its own"
        );
        // Any duplicate or swallowed signal shows up in the counts, read
        // once the child is long done so every delivery has landed.
        assert_eq!(
            std::fs::read(dir.join("terms")).expect("the TERM count"),
            b"x",
            "exactly one TERM reached the child"
        );
        assert_eq!(
            std::fs::read(dir.join("ints")).expect("the INT count"),
            b"x",
            "exactly one INT reached the child"
        );
        let _ = std::fs::write(dir.join("guardian_pid"), supervision.pid.to_string());
    }

    fn open_append(path: &Path) -> std::os::fd::OwnedFd {
        rustix::fs::open(
            path,
            OFlags::CREATE | OFlags::WRONLY | OFlags::APPEND,
            Mode::RUSR | Mode::WUSR,
        )
        .expect("open a counting file")
    }

    /// The driver of the no-replacement rule: a process started after the
    /// foreground's exit never becomes the ancestor of the orphaned
    /// descendant.
    #[test]
    fn post_exit_replacement_never_receives_ancestry() {
        let dir = temp_dir("replacement");
        run_worker("worker_post_exit_replacement", &dir);
        if dir.join("unsupported").exists() {
            eprintln!("skipping: this kernel does not take the child subreaper attribute");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let guardian = read_u32_file(&dir.join("guardian_pid")).expect("guardian pid file");
        assert!(
            wait_for(&dir.join("daemon_pid"), 5),
            "the daemon never reported its pid"
        );
        let daemon = read_u32_file(&dir.join("daemon_pid")).expect("daemon pid file");

        // The foreground is gone and the caller gone with it; the daemon
        // may not even have been re-parented yet. A process started now is
        // exactly the replacement the launch must never hand ancestry to.
        let mut replacement = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn the replacement");
        let mut adopted = false;
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline && alive(daemon) {
            if let Some(ppid) = ppid_of(daemon) {
                assert_ne!(
                    ppid,
                    replacement.id(),
                    "the replacement became the ancestor of the orphaned daemon"
                );
                assert_ne!(ppid, 1, "the daemon reached init instead of the guardian");
                adopted |= ppid == guardian;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            adopted,
            "the guardian was never the orphaned daemon's ancestor"
        );

        assert!(
            wait_for(&dir.join("released"), 15),
            "the guardian never released the launch"
        );
        assert!(gone(guardian, 15), "the guardian never exited");
        assert!(gone(daemon, 5), "the orphaned daemon was left a zombie");
        let _ = replacement.kill();
        let _ = replacement.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Runs in its own process: supervises a foreground that exits while
    /// a double forked daemon lives, and hands the pids to the driver.
    #[test]
    #[ignore = "driven by post_exit_replacement_never_receives_ancestry, in a quiet process"]
    fn worker_post_exit_replacement() {
        let dir = worker_dir();
        if !subreaper_supported() {
            let _ = std::fs::write(dir.join("unsupported"), b"");
            return;
        }
        let lease = lease_lock(&dir);
        let release = {
            let dir = dir.clone();
            move |_lease: std::fs::File| {
                let _ = std::fs::write(dir.join("released"), b"1");
            }
        };
        let start = {
            let dir = dir.clone();
            move || {
                fork_daemon(&dir, 6, 9);
                std::process::exit(4);
            }
        };
        let supervision = super::spawn(lease, start, release).expect("spawn the guardian");
        let outcome = super::receive(&supervision.status).expect("the reported status");
        assert_eq!(outcome, Outcome::Exit(4), "the original foreground status");
        let _ = std::fs::write(dir.join("guardian_pid"), supervision.pid.to_string());
    }

    /// The driver of the prctl check: the attribute reads back set.
    #[test]
    fn the_subreaper_attribute_reads_back_set() {
        let dir = temp_dir("prctl");
        run_worker("worker_subreaper_prctl", &dir);
        if dir.join("unsupported").exists() {
            eprintln!("skipping: this kernel does not take the child subreaper attribute");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        assert!(
            dir.join("subreaper").exists(),
            "the subreaper attribute did not read back as set"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Runs in its own process: sets the attribute, reads it back, clears
    /// it, reads that back too.
    #[test]
    #[ignore = "driven by the_subreaper_attribute_reads_back_set, in a quiet process"]
    fn worker_subreaper_prctl() {
        let dir = worker_dir();
        if super::mark_subreaper().is_err() {
            let _ = std::fs::write(dir.join("unsupported"), b"");
            return;
        }
        assert!(
            child_subreaper().expect("prctl").is_some(),
            "the attribute did not read back as set"
        );
        let _ = set_child_subreaper(None);
        assert!(
            child_subreaper().expect("prctl").is_none(),
            "the attribute did not read back as cleared"
        );
        let _ = std::fs::write(dir.join("subreaper"), b"1");
    }

    /// The records round-trip through the pipe encoding, byte for byte.
    #[test]
    fn outcome_records_round_trip() {
        for outcome in [
            Outcome::Exit(0),
            Outcome::Exit(255),
            Outcome::Signaled(1),
            Outcome::Signaled(31),
            Outcome::Other,
            Outcome::Unavailable,
        ] {
            assert_eq!(decode_record(encode_record(outcome)), outcome);
        }
        assert_eq!(
            encode_record(Outcome::Unavailable),
            super::UNAVAILABLE_RECORD
        );
    }

    /// A kind byte nothing wrote reads as the catch-all outcome.
    #[test]
    fn an_unknown_record_kind_reads_as_other() {
        assert_eq!(decode_record([9, 200]), Outcome::Other);
        assert_eq!(decode_record([2, 7]), Outcome::Other);
    }

    /// The exit code conversion is the one the launch modes have always
    /// applied.
    #[test]
    fn exit_codes_match_the_launch_conversion() {
        assert_eq!(Outcome::Exit(0).exit_code(), 0);
        assert_eq!(Outcome::Exit(255).exit_code(), 255);
        assert_eq!(Outcome::Signaled(15).exit_code(), 143);
        assert_eq!(Outcome::Signaled(6).exit_code(), 134);
        assert_eq!(Outcome::Other.exit_code(), 1);
        assert_eq!(Outcome::Unavailable.exit_code(), 1);
    }
}

//! Concurrent launches of one package running from one FUSE mount.
//!
//! The first launch mounts the package, hands the mount to a server
//! process of its own and runs the app from it. A launch that finds that
//! server alive runs from its mount too, instead of mounting the package
//! again and decompressing everything a second time. A mount made through
//! `fusermount3` is in the host's namespace, so joining it takes nothing.
//! A mount made in a private namespace is reached by entering the server's
//! user and mount namespaces, which only a launch that sees the same
//! mounts the server does may do.
//!
//! Three files under the private runtime directory hold it together (see
//! `paths::share_files`):
//!
//! - The record names the server: how it mounted, its pid and start time,
//!   and the whole package id. A launch locks it exclusively while it
//!   decides whether to join or start a server, and the first launch holds
//!   that lock until it has put its library links in place and execs the
//!   app, so no launch finds a half-made mount or starts a second server.
//! - The lease is held shared by every launch running from the mount, and
//!   inherited by the app and everything it starts.
//! - The mountpoint's claim keeps the directory from being swept while the
//!   server lives.
//!
//! The server leaves once it can lock the lease and the record exclusively
//! and no process runs out of the mount, which catches an app that
//! daemonized and closed what it inherited. It takes the mount down and
//! removes the record and the lease before it lets go of them.
//!
//! A package file replaced on disk has a new package id, so a launch of it
//! never finds the old server's record, and a launch whose library links
//! differ from what the mount serves mounts its own.

use std::fs::File;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::FlockOperation;
use rustix::io::FdFlags;
use rustix::runtime::{Fork, kernel_fork};
use rustix::thread::{LinkNameSpaceType, UnshareFlags, move_into_link_name_space, unshare_unsafe};

use super::{Request, mount};
use crate::launch::Links;
use crate::loader::PackageData;
use crate::paths::Mountpoint;

/// Why a launch did not run from a shared mount.
pub(crate) enum Declined {
    /// The launch mounts the package for itself instead.
    Private,
    /// Nothing could be mounted at all, so the next mode is tried.
    Unmountable,
}

/// Whether this launch may share a mount. `ONELF_FUSE_NO_SHARE` turns it
/// off. A traced launch never shares: the trace is recorded by the server,
/// which would record what every instance reads.
pub(crate) fn wanted() -> bool {
    let set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty() && v != "0");
    !set("ONELF_FUSE_NO_SHARE") && std::env::var_os("ONELF_TRACE").is_none_or(|v| v.is_empty())
}

/// What the record says about the server.
struct Record {
    /// Whether the mount is in the server's own namespace, rather than the
    /// host's.
    namespace: bool,
    pid: i32,
    /// The server's start time, which tells it apart from a process that
    /// took its pid after it exited.
    start: u64,
    /// The whole package id, in hex.
    id: String,
}

impl Record {
    fn parse(text: &str) -> Option<Self> {
        let mut fields = text.split_whitespace();
        let namespace = match fields.next()? {
            "ns" => true,
            "host" => false,
            _ => return None,
        };
        Some(Self {
            namespace,
            pid: fields.next()?.parse().ok().filter(|pid| *pid > 0)?,
            start: fields.next()?.parse().ok()?,
            id: fields.next()?.to_string(),
        })
    }

    fn render(&self) -> String {
        let how = if self.namespace { "ns" } else { "host" };
        format!("{how} {} {} {}\n", self.pid, self.start, self.id)
    }

    /// Whether the server the record names is still running.
    fn live(&self) -> bool {
        started_at(self.pid) == Some(self.start)
    }
}

/// The record in `file`, if it holds one.
fn read_record(file: &File) -> Option<Record> {
    let mut buf = [0u8; 256];
    let n = file.read_at(&mut buf, 0).ok()?;
    Record::parse(std::str::from_utf8(&buf[..n]).ok()?)
}

/// When the process `pid` started, in clock ticks since boot, unless it
/// has exited. Together with the pid this names one process.
fn started_at(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may hold anything, so the fields are counted from
    // the parenthesis that ends it. The state comes first, the start time
    // nineteen fields after it.
    let (_, rest) = stat.rsplit_once(") ")?;
    let mut fields = rest.split_whitespace();
    if matches!(fields.next()?, "Z" | "X") {
        return None;
    }
    fields.nth(18)?.parse().ok()
}

/// Whether `file` is the file at `path` rather than one removed from there.
fn same_file(file: &File, path: &Path) -> bool {
    match (file.metadata(), std::fs::metadata(path)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// The record file at `path`, locked exclusively.
///
/// A server that leaves removes the file while it holds the lock, so a
/// launch that waited on it finds the lock on a file nobody else will
/// open again, and opens the new one instead.
fn lock_record(path: &Path) -> Option<File> {
    for _ in 0..16 {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .ok()?;
        rustix::fs::flock(&file, FlockOperation::LockExclusive).ok()?;
        if same_file(&file, path) {
            return Some(file);
        }
    }
    None
}

/// The lease at `path`, held shared on a descriptor the app inherits.
/// Never created here: a lease the server does not watch protects nothing.
fn take_lease(path: &Path) -> Option<File> {
    let fd = rustix::fs::open(path, rustix::fs::OFlags::RDWR, rustix::fs::Mode::empty()).ok()?;
    let lease = File::from(fd);
    rustix::fs::flock(&lease, FlockOperation::LockShared).ok()?;
    Some(lease)
}

/// The files of one package's shared mount.
struct Share {
    mountpoint: PathBuf,
    record: PathBuf,
    lease: PathBuf,
    id: String,
}

/// Run the package from the mount another launch of it serves, or mount
/// it and serve it for every launch that follows. Returns only when this
/// launch does neither.
pub(crate) fn launch(pkg: &mut PackageData, req: &Request, needs_setuid: bool) -> Declined {
    let manifest = &pkg.manifest;
    let id = crate::cache::hex(&manifest.header.package_id);
    let dir_name = crate::paths::shared_dir_name(manifest.name(), &manifest.header.package_id);
    let (Some(base), Some((record, lease))) = (
        crate::paths::private_dir(),
        crate::paths::share_files(&dir_name),
    ) else {
        return Declined::Private;
    };
    let share = Share {
        mountpoint: base.join(&dir_name),
        record,
        lease,
        id,
    };
    let Some(record_lock) = lock_record(&share.record) else {
        return Declined::Private;
    };
    match read_record(&record_lock).filter(Record::live) {
        Some(record) if record.id == share.id => {
            join(pkg, req, &record, &share, record_lock, needs_setuid)
        }
        // Another package whose name and id begin the same way.
        Some(_) => Declined::Private,
        None => create(pkg, req, &share, record_lock, needs_setuid),
    }
}

/// Run from the mount the server in `record` serves.
fn join(
    pkg: &PackageData,
    req: &Request,
    record: &Record,
    share: &Share,
    record_lock: File,
    needs_setuid: bool,
) -> Declined {
    // Taken while the record is locked, so the server cannot settle on
    // leaving in between.
    let Some(lease) = take_lease(&share.lease) else {
        return Declined::Private;
    };
    drop(record_lock);
    let joined = if record.namespace {
        !super::skips_namespace(needs_setuid) && enter_namespaces(record, &share.mountpoint)
    } else {
        crate::paths::is_mountpoint(&share.mountpoint) && share.mountpoint.read_dir().is_ok()
    };
    if !joined {
        return Declined::Private;
    }
    req.setup_portable();
    crate::launch::exec_served(&req.launch(
        pkg,
        &share.mountpoint,
        record.namespace,
        Links::Served,
    ));
    // The links that mount serves were placed for a launch that resolved
    // the host differently, and only that launch could have chosen them.
    drop(lease);
    eprintln!(
        "onelf-rt: fuse: the running instance's mount serves other host \
         libraries than this launch needs; mounting its own"
    );
    Declined::Private
}

/// Enter the user and mount namespaces of the server in `record`, then a
/// mount namespace of this launch's own copied from it, keeping the
/// working directory. False when the launch cannot run from there.
///
/// The server's namespace is a copy of the mounts its first launch saw. A
/// launch that sees others, because it started inside another namespace
/// or because the host mounted something since and does not propagate
/// mounts into copies, would lose a path it was handed. Such a launch
/// mounts its own.
fn enter_namespaces(record: &Record, mountpoint: &Path) -> bool {
    let server = PathBuf::from(format!("/proc/{}", record.pid));
    if std::fs::read_link("/proc/self/root").ok().as_deref() != Some(Path::new("/")) {
        return false;
    }
    let ours = mount_table(Path::new("/proc/self/mountinfo"), mountpoint);
    if ours.is_none() || ours != mount_table(&server.join("mountinfo"), mountpoint) {
        return false;
    }
    let Ok(cwd) = std::env::current_dir() else {
        return false;
    };
    let there = server
        .join("root")
        .join(cwd.strip_prefix("/").unwrap_or(&cwd));
    let same_dir = match (std::fs::metadata("."), std::fs::metadata(&there)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    };
    if !same_dir {
        return false;
    }

    let open = |kind: &str| File::open(server.join("ns").join(kind)).ok();
    let (Some(user), Some(mnt)) = (open("user"), open("mnt")) else {
        return false;
    };
    // Checked after opening, so the namespaces are the server's and not
    // those of a process that took its pid.
    if !record.live() {
        return false;
    }
    let in_user = File::open("/proc/self/ns/user").is_ok_and(|own| {
        matches!((own.metadata(), user.metadata()),
            (Ok(a), Ok(b)) if a.dev() == b.dev() && a.ino() == b.ino())
    });
    if !in_user && move_into_link_name_space(user.as_fd(), Some(LinkNameSpaceType::User)).is_err() {
        return false;
    }
    if move_into_link_name_space(mnt.as_fd(), Some(LinkNameSpaceType::Mount)).is_err() {
        return false;
    }
    // Entering a mount namespace moves to its root. The directory was
    // shown to be the same one there, so failing to return to it means
    // something changed underneath, and the app is not run elsewhere.
    if let Err(e) = std::env::set_current_dir(&cwd) {
        eprintln!(
            "onelf-rt: fuse: cannot return to {} after joining the running \
             instance: {e}",
            cwd.display()
        );
        std::process::exit(1);
    }
    // Mounts this launch makes, the host libraries bound over bundled
    // ones among them, stay out of the namespace every launch joins.
    crate::paths::is_mountpoint(mountpoint)
        && mountpoint.read_dir().is_ok()
        && unsafe { unshare_unsafe(UnshareFlags::NEWNS) }.is_ok()
}

/// The mounts `mountinfo` lists, as device, root and mountpoint, sorted.
///
/// Left out are the shared mount at `mountpoint` and anything under it,
/// and the mounts of onelf instances run from another runtime directory:
/// no launch from this one reads those, and they come and go on their own.
fn mount_table(mountinfo: &Path, mountpoint: &Path) -> Option<Vec<(String, String, String)>> {
    let text = std::fs::read_to_string(mountinfo).ok()?;
    let base = mountpoint.parent()?;
    let mut table: Vec<_> = text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(' ');
            let device = fields.nth(2)?;
            let root = fields.next()?;
            let at = fields.next()?;
            let path = PathBuf::from(at.replace("\\040", " "));
            let elsewhere = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("onelf-"))
                && path.parent() != Some(base);
            (!path.starts_with(mountpoint) && !elsewhere)
                .then(|| (device.to_string(), root.to_string(), at.to_string()))
        })
        .collect();
    table.sort();
    Some(table)
}

/// Mount the package, start the server that serves it to every launch,
/// and run the app from it. Holding `record_lock` until the exec keeps
/// every other launch waiting until this one's links are served.
fn create(
    pkg: &mut PackageData,
    req: &Request,
    share: &Share,
    record_lock: File,
    needs_setuid: bool,
) -> Declined {
    let dir_name = share
        .mountpoint
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let Some(claim) = crate::paths::create_shared_mountpoint(dir_name) else {
        return Declined::Private;
    };
    let mountpoint = claim.path().to_path_buf();
    if crate::paths::is_mountpoint(&mountpoint) {
        // A live mount nobody records is not one to run from or stack on.
        if mountpoint.read_dir().is_ok() {
            claim.release();
            return Declined::Private;
        }
        mount::fuse_unmount(&mountpoint);
    }
    let (fuse_fd, namespace) = match super::mount_in_namespace(&mountpoint, needs_setuid) {
        Ok(fd) => (fd, true),
        Err(ns_err) => match super::mount_with_helper(&mountpoint, &ns_err, needs_setuid) {
            Some(fd) => (fd, false),
            None => {
                claim.release();
                return Declined::Unmountable;
            }
        },
    };
    let _ = rustix::io::fcntl_setfd(&fuse_fd, FdFlags::CLOEXEC);

    // What a killed server ran may still hold its lease. A fresh file
    // leaves those holders out of this server's count.
    let _ = std::fs::remove_file(&share.lease);
    let lease = crate::paths::open_lock_inheritable(&share.lease)
        .ok()
        .filter(|l| rustix::fs::flock(l, FlockOperation::LockShared).is_ok());
    // Hangs up when this launch execs the app, which is when the server
    // has its links and starts asking whether anyone still needs it.
    let launched = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC);
    let channel = crate::links::channel();
    let (Some(lease), Ok((launched_r, launched_w)), Ok((links_tx, links_rx))) =
        (lease, launched, channel)
    else {
        super::cleanup_mountpoint(claim, namespace);
        return Declined::Private;
    };

    let started = match unsafe { kernel_fork() } {
        Ok(Fork::Child(_)) => {
            // Through an intermediate, so the server is not a child of the
            // app this process becomes: an app that waits for any child
            // would reap it, or wait for it.
            if let Ok(Fork::Child(_)) = unsafe { kernel_fork() } {
                drop(launched_w);
                drop(links_tx);
                serve(
                    pkg,
                    share,
                    Server {
                        fuse_fd,
                        record_lock,
                        lease,
                        launched: launched_r,
                        links: links_rx,
                        claim,
                        namespace,
                    },
                );
            }
            std::process::exit(0)
        }
        Ok(Fork::ParentOf(child)) => loop {
            match rustix::process::waitpid(Some(child), rustix::process::WaitOptions::empty()) {
                Ok(Some((_, status))) => break status.exit_status() == Some(0),
                Err(rustix::io::Errno::INTR) => continue,
                _ => break false,
            }
        },
        Err(_) => false,
    };
    if !started {
        super::cleanup_mountpoint(claim, namespace);
        return Declined::Private;
    }

    // The server holds the mount from here. A copy of the connection kept
    // here would outlive a server that died, and the app would hang on the
    // mount instead of failing.
    drop(fuse_fd);
    drop(launched_r);
    drop(links_rx);
    drop(claim);
    // The host libraries bound over bundled ones stay out of the namespace
    // every later launch joins.
    let private_ns = namespace && unsafe { unshare_unsafe(UnshareFlags::NEWNS) }.is_ok();
    req.setup_portable();
    // The record lock and the pipe close at the exec. The lease does not.
    let _held = (record_lock, lease, launched_w);
    crate::launch::exec(&req.launch(pkg, &mountpoint, private_ns, Links::Server(&links_tx)))
}

/// What the first launch hands its server.
struct Server {
    fuse_fd: OwnedFd,
    record_lock: File,
    lease: File,
    launched: OwnedFd,
    links: crate::links::Receiver,
    claim: Mountpoint,
    namespace: bool,
}

/// Serve the shared mount until nobody uses it, then take it down and
/// remove the record and the lease. Never returns.
fn serve(pkg: &mut PackageData, share: &Share, server: Server) -> ! {
    let Server {
        fuse_fd,
        record_lock,
        lease,
        launched,
        links,
        claim,
        namespace,
    } = server;
    super::detach_from_caller();
    // Off whatever disk the launch was started from, which would otherwise
    // stay busy for as long as this serves.
    let _ = std::env::set_current_dir("/");

    let me = rustix::process::getpid().as_raw_nonzero().get();
    let record = Record {
        namespace,
        pid: me,
        start: started_at(me).unwrap_or_default(),
        id: share.id.clone(),
    };
    let _ = record_lock
        .set_len(0)
        .and_then(|()| record_lock.write_all_at(record.render().as_bytes(), 0));
    // Descriptors of the server's own, since a lock belongs to the open
    // file description that took it: these ask about everyone else's.
    let own_record = File::open(&share.record).ok();
    let own_lease = File::open(&share.lease).ok();
    // The launch keeps its locks. Closing these copies does not release
    // them, since the launch holds the same descriptions.
    drop(record_lock);
    drop(lease);

    let mut state = super::fs::FuseState::new(
        &pkg.manifest,
        &mut pkg.file,
        &pkg.footer,
        pkg.dict.as_deref(),
    );
    state.expect_links(links);
    let mut buf = vec![0u8; 1024 * 1024 + 4096];
    state.run_loop(&fuse_fd, Some(&launched), None::<&OwnedFd>, &mut buf);
    drop(launched);
    let mut settled = false;
    state.serve_detached(&fuse_fd, &mut buf, || {
        // A mount taken down from outside, such as `fusermount3 -u -z`,
        // is finished whoever still holds the lease: no launch can join
        // it, and a process left inside a lazily detached tree would
        // otherwise keep the server for as long as it lives.
        if !super::is_live_mount(claim.path()) {
            return true;
        }
        settled = idle(own_lease.as_ref(), own_record.as_ref(), claim.path());
        settled
    });
    drop(fuse_fd);
    // Unmounted from outside: no launch may find the record any more.
    if !settled && let Some(own) = &own_record {
        let _ = rustix::fs::flock(own, FlockOperation::LockExclusive);
    }

    if namespace {
        mount::fuse_unmount_direct(claim.path());
    } else {
        mount::fuse_unmount(claim.path());
    }
    let names_me = own_record
        .as_ref()
        .and_then(read_record)
        .is_some_and(|r| r.pid == record.pid && r.start == record.start);
    if names_me {
        if own_lease
            .as_ref()
            .is_some_and(|l| same_file(l, &share.lease))
        {
            let _ = std::fs::remove_file(&share.lease);
        }
        if own_record
            .as_ref()
            .is_some_and(|r| same_file(r, &share.record))
        {
            let _ = std::fs::remove_file(&share.record);
        }
    }
    claim.release();
    std::process::exit(0);
}

/// Whether nothing needs the mount any more: nobody holds the lease, no
/// launch is deciding whether to join, and no process runs out of the
/// mount. When it says so, both locks are held, so no launch can join
/// until this server is gone.
fn idle(lease: Option<&File>, record: Option<&File>, mountpoint: &Path) -> bool {
    let take = |f: Option<&File>| {
        f.is_none_or(|f| rustix::fs::flock(f, FlockOperation::NonBlockingLockExclusive).is_ok())
    };
    let give = |f: Option<&File>| {
        if let Some(f) = f {
            let _ = rustix::fs::flock(f, FlockOperation::Unlock);
        }
    };
    if !take(lease) {
        return false;
    }
    if !take(record) {
        give(lease);
        return false;
    }
    if super::mount_in_use(mountpoint) {
        give(record);
        give(lease);
        return false;
    }
    true
}

/// Remove the record and lease of the shared mount `dir_name` under
/// `base` when no server is running for them, which a server that was
/// killed leaves behind.
pub(crate) fn reclaim_stale(base: &Path, dir_name: &str) {
    let (record, lease) = crate::paths::share_files_in(base, dir_name);
    let Ok(file) = File::open(&record) else {
        return;
    };
    if rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive).is_err()
        || !same_file(&file, &record)
        || read_record(&file).is_some_and(|r| r.live())
    {
        return;
    }
    let _ = std::fs::remove_file(&lease);
    let _ = std::fs::remove_file(&record);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_round_trips() {
        let record = Record {
            namespace: true,
            pid: 4242,
            start: 99,
            id: "ab".repeat(32),
        };
        let back = Record::parse(&record.render()).expect("a record");
        assert!(back.namespace);
        assert_eq!((back.pid, back.start, back.id), (4242, 99, "ab".repeat(32)));
        assert!(Record::parse("").is_none());
        assert!(Record::parse("ns 0 1 x").is_none());
        assert!(Record::parse("other 1 1 x").is_none());
    }

    #[test]
    fn a_record_names_this_process_while_it_runs() {
        let me = std::process::id() as i32;
        let start = started_at(me).expect("our own start time");
        let record = |start| Record {
            namespace: false,
            pid: me,
            start,
            id: String::new(),
        };
        assert!(record(start).live());
        assert!(!record(start + 1).live(), "another process with our pid");
    }

    #[test]
    fn the_shared_mount_and_other_runtimes_instances_are_left_out() {
        let dir = std::env::temp_dir().join(format!("onelf-share-mi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let info = dir.join("mountinfo");
        let base = "/run/user/1000";
        std::fs::write(
            &info,
            format!(
                "1 0 0:1 / / rw - ext4 /dev/a rw\n\
                 2 1 0:9 / {base}/onelf-app-0011 ro - fuse fuse ro\n\
                 3 2 0:2 /lib {base}/onelf-app-0011/lib/x rw - ext4 /dev/a rw\n\
                 4 1 0:7 / {base}/onelf-set-ab ro - fuse fuse ro\n\
                 5 1 0:8 / /tmp/elsewhere/onelf-other-22 ro - fuse fuse ro\n"
            ),
        )
        .unwrap();
        let table = mount_table(&info, &Path::new(base).join("onelf-app-0011")).unwrap();
        let points: Vec<&str> = table.iter().map(|(_, _, at)| at.as_str()).collect();
        assert_eq!(points, vec!["/", "/run/user/1000/onelf-set-ab"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

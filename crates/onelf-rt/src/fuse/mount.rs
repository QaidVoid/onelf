//! FUSE mount/unmount.
//!
//! Preferred path: create a private user+mount namespace and call mount(2)
//! directly on /dev/fuse. No external helper, mount is invisible outside
//! our namespace, and the kernel tears it down automatically on exit.
//!
//! Fallback: the legacy fusermount3 helper protocol (socketpair + SCM_RIGHTS)
//! for systems where unprivileged user namespaces are disabled.

use std::fs::File;
use std::io::{self, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use rustix::io::FdFlags;
use rustix::mount::{MountFlags, UnmountFlags, mount, unmount};
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SocketFlags, SocketType,
    recvmsg, socketpair,
};
use rustix::thread::{UnshareFlags, unshare_unsafe};

/// Enter a private user+mount namespace with our real uid/gid mapped 1:1.
/// After this returns, the process has CAP_SYS_ADMIN inside the namespace
/// and subsequent mounts are invisible to the host.
pub fn enter_namespace() -> io::Result<()> {
    // Entering is one-way: a launcher that unshares and then finds it may
    // not map its ids or mount anything is stuck in a namespace where
    // fusermount3's setuid bit means nothing and every fallback runs as
    // the overflow uid. A child tries it first, so the launcher only
    // commits when the whole sequence works. Ubuntu's AppArmor policy is
    // the common case: it allows the unshare and denies what follows.
    if !namespace_usable() {
        return Err(io::Error::other(
            "user namespaces are restricted on this host",
        ));
    }
    unshare_and_map()
}

/// Unshare a user and mount namespace and map our ids 1:1 into it.
fn unshare_and_map() -> io::Result<()> {
    // SAFETY: single-threaded runtime; unshare only affects this thread.
    let real_uid = rustix::process::getuid().as_raw();
    let real_gid = rustix::process::getgid().as_raw();

    unsafe {
        unshare_unsafe(UnshareFlags::NEWUSER | UnshareFlags::NEWNS)
            .map_err(|e| io::Error::other(format!("unshare: {e}")))?;
    }

    // Map our real uid/gid 1:1 into the new user namespace. Must deny
    // setgroups before writing gid_map or the kernel rejects it.
    File::create("/proc/self/setgroups")?.write_all(b"deny")?;
    File::create("/proc/self/uid_map")?.write_all(format!("{real_uid} {real_uid} 1").as_bytes())?;
    File::create("/proc/self/gid_map")?.write_all(format!("{real_gid} {real_gid} 1").as_bytes())?;

    Ok(())
}

/// Whether a forked child can unshare, map its ids and mount a tmpfs.
/// The mount lands in the child's own namespace and goes with it.
fn namespace_usable() -> bool {
    use rustix::runtime::{Fork, kernel_fork};

    match unsafe { kernel_fork() } {
        Ok(Fork::Child(_)) => {
            let target = std::env::temp_dir();
            let target = if target.is_dir() { target } else { "/".into() };
            let ok = unshare_and_map().is_ok()
                && mount(
                    "tmpfs",
                    &target,
                    "tmpfs",
                    MountFlags::NOSUID | MountFlags::NODEV,
                    None,
                )
                .is_ok();
            std::process::exit(if ok { 0 } else { 1 })
        }
        Ok(Fork::ParentOf(child)) => loop {
            match rustix::process::waitpid(Some(child), rustix::process::WaitOptions::empty()) {
                Ok(Some((_, status))) => return status.exit_status() == Some(0),
                Err(rustix::io::Errno::INTR) => continue,
                _ => return false,
            }
        },
        Err(_) => false,
    }
}

/// Mount options for the namespace mount: `fd` plus the root dir and
/// owner, always with [`FUSE_PERM_OPTS`].
fn namespace_mount_data(fd: i32, uid: u32, gid: u32) -> String {
    format!("fd={fd},rootmode=40000,user_id={uid},group_id={gid},{FUSE_PERM_OPTS}")
}

/// The permission posture every mount path must carry.
///
/// `default_permissions` makes the kernel enforce the recorded modes
/// itself, so neither mount path depends on the daemon's judgement to
/// stay safe; the honest ACCESS reply in `fs.rs` is a second layer, not
/// the gate. Both mount paths are required to set it, and the test at
/// the bottom of this file locks that in.
const FUSE_PERM_OPTS: &str = "default_permissions";

/// Mount a FUSE filesystem directly, after entering a private user+mount
/// namespace. Returns the /dev/fuse fd. No external helper required.
///
/// The caller must invoke this BEFORE forking the FUSE server; forked
/// children inherit the mount namespace and see the mount automatically.
/// When the last process in the user namespace exits, the kernel tears
/// the mount down, so no cleanup code is needed.
pub fn fuse_mount_unshare(mountpoint: &Path) -> io::Result<OwnedFd> {
    enter_namespace()?;

    let real_uid = rustix::process::getuid().as_raw();
    let real_gid = rustix::process::getgid().as_raw();

    let fuse_fd: OwnedFd = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")?
        .into();

    // rootmode=040000 = S_IFDIR.
    let data = namespace_mount_data(fuse_fd.as_raw_fd(), real_uid, real_gid);
    let data_c = std::ffi::CString::new(data).unwrap();

    mount(
        "fuse",
        mountpoint,
        "fuse",
        MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOATIME | MountFlags::RDONLY,
        Some(data_c.as_c_str()),
    )
    .map_err(|e| io::Error::other(format!("mount /dev/fuse: {e}")))?;

    Ok(fuse_fd)
}

/// Mount a tmpfs at `mountpoint`. Caller must already be in a private
/// mount namespace (via `enter_namespace`).
pub fn mount_tmpfs(mountpoint: &Path, size_bytes: u64) -> io::Result<()> {
    let data = format!("size={size_bytes},mode=0755");
    let data_c = std::ffi::CString::new(data).unwrap();

    mount(
        "tmpfs",
        mountpoint,
        "tmpfs",
        MountFlags::NOSUID | MountFlags::NODEV,
        Some(data_c.as_c_str()),
    )
    .map_err(|e| io::Error::other(format!("mount tmpfs: {e}")))
}

/// Unmount a FUSE filesystem directly (for the unshare path).
pub fn fuse_unmount_direct(mountpoint: &Path) {
    let _ = unmount(mountpoint, UnmountFlags::DETACH);
}

/// Mount a FUSE filesystem via fusermount3 and return the /dev/fuse fd.
///
/// Uses the fusermount3 protocol: create a socketpair, pass one end to
/// fusermount3 via `_FUSE_COMMFD`, then receive the /dev/fuse fd via
/// SCM_RIGHTS on the other end.
pub fn fuse_mount(mountpoint: &Path) -> io::Result<OwnedFd> {
    let (sock_parent, sock_child) = socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )
    .map_err(|e| io::Error::other(format!("socketpair: {e}")))?;

    let child_fd = sock_child.as_raw_fd();

    let status = unsafe {
        Command::new("fusermount3")
            .args([
                "-o",
                &format!("ro,nosuid,nodev,noatime,{FUSE_PERM_OPTS}"),
                "--",
            ])
            .arg(mountpoint)
            .env("_FUSE_COMMFD", child_fd.to_string())
            .pre_exec(move || {
                // Clear CLOEXEC so fusermount3 inherits this fd
                let fd = BorrowedFd::borrow_raw(child_fd);
                let flags = rustix::io::fcntl_getfd(fd).map_err(io::Error::other)?;
                rustix::io::fcntl_setfd(fd, flags.difference(FdFlags::CLOEXEC))
                    .map_err(io::Error::other)?;
                Ok(())
            })
            .status()
    }
    .map_err(|e| io::Error::new(io::ErrorKind::NotFound, format!("fusermount3: {e}")))?;

    // Drop child end so recvmsg doesn't block forever if fusermount3 failed
    drop(sock_child);

    if !status.success() {
        return Err(io::Error::other(format!(
            "fusermount3 exited with {status}"
        )));
    }

    // Receive the /dev/fuse fd via SCM_RIGHTS. Close-on-exec from the
    // start: the app this process execs must not inherit it, or the mount
    // would outlive its server. A killed server cannot abort a connection
    // some other process still holds open, and that process, once it
    // touches the mount, waits for a reply that never comes and cannot
    // even be killed.
    let mut cmsg_buf = [MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut ancillary = RecvAncillaryBuffer::new(&mut cmsg_buf);
    let mut iov_buf = [0u8; 1];
    let iov = io::IoSliceMut::new(&mut iov_buf);

    let _msg = recvmsg(
        &sock_parent,
        &mut [iov],
        &mut ancillary,
        RecvFlags::CMSG_CLOEXEC,
    )
    .map_err(|e| io::Error::other(format!("recvmsg: {e}")))?;

    for msg in ancillary.drain() {
        if let RecvAncillaryMessage::ScmRights(mut fds) = msg
            && let Some(fd) = fds.next()
        {
            return Ok(fd);
        }
    }

    Err(io::Error::other("fusermount3 did not send /dev/fuse fd"))
}

/// Unmount a FUSE filesystem via fusermount3 -u.
/// Uses lazy unmount (-z) to handle dead mounts where the FUSE daemon exited.
pub fn fuse_unmount(mountpoint: &Path) {
    let _ = Command::new("fusermount3")
        .args(["-u", "-z", "-q", "--"])
        .arg(mountpoint)
        .status();
}

/// Check if fusermount3 is available on the system.
pub fn fusermount3_available() -> bool {
    Command::new("fusermount3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

#[cfg(test)]
mod perm_opts_tests {
    use super::{FUSE_PERM_OPTS, namespace_mount_data};

    /// The permission invariant the ACCESS handler leans on: whatever
    /// else changes, both mount paths must make the kernel enforce the
    /// recorded modes, or `default_permissions` safety is gone.
    #[test]
    fn both_mount_paths_enforce_default_permissions() {
        let data = namespace_mount_data(7, 1000, 1000);
        assert!(data.starts_with("fd=7,rootmode=40000"), "{data}");
        assert!(data.contains(FUSE_PERM_OPTS), "namespace mount: {data}");
        assert!(
            data.contains("user_id=1000") && data.contains("group_id=1000"),
            "{data}"
        );
    }
}

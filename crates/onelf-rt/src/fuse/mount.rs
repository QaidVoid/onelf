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

/// Mount a FUSE filesystem directly, after entering a private user+mount
/// namespace. Returns the /dev/fuse fd. No external helper required.
///
/// The caller must invoke this BEFORE forking the FUSE server; forked
/// children inherit the mount namespace and see the mount automatically.
/// When the last process in the user namespace exits, the kernel tears
/// the mount down — no cleanup code needed.
pub fn fuse_mount_unshare(mountpoint: &Path) -> io::Result<OwnedFd> {
    enter_namespace()?;

    let real_uid = rustix::process::getuid().as_raw();
    let real_gid = rustix::process::getgid().as_raw();

    let fuse_fd: OwnedFd = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")?
        .into();

    // rootmode=040000 = S_IFDIR; allow_other lets the child process (running
    // as the same uid inside the ns) access the mount through the inode.
    let data = format!(
        "fd={},rootmode=40000,user_id={real_uid},group_id={real_gid}",
        fuse_fd.as_raw_fd()
    );
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
    .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("socketpair: {e}")))?;

    let child_fd = sock_child.as_raw_fd();

    let status = unsafe {
        Command::new("fusermount3")
            .args(["-o", "ro,nosuid,nodev,noatime,default_permissions", "--"])
            .arg(mountpoint)
            .env("_FUSE_COMMFD", child_fd.to_string())
            .pre_exec(move || {
                // Clear CLOEXEC so fusermount3 inherits this fd
                let fd = BorrowedFd::borrow_raw(child_fd);
                let flags = rustix::io::fcntl_getfd(fd)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                rustix::io::fcntl_setfd(fd, flags.difference(FdFlags::CLOEXEC))
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                Ok(())
            })
            .status()
    }
    .map_err(|e| io::Error::new(io::ErrorKind::NotFound, format!("fusermount3: {e}")))?;

    // Drop child end so recvmsg doesn't block forever if fusermount3 failed
    drop(sock_child);

    if !status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("fusermount3 exited with {status}"),
        ));
    }

    // Receive the /dev/fuse fd via SCM_RIGHTS
    let mut cmsg_buf = [MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut ancillary = RecvAncillaryBuffer::new(&mut cmsg_buf);
    let mut iov_buf = [0u8; 1];
    let iov = io::IoSliceMut::new(&mut iov_buf);

    let _msg = recvmsg(&sock_parent, &mut [iov], &mut ancillary, RecvFlags::empty())
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("recvmsg: {e}")))?;

    for msg in ancillary.drain() {
        if let RecvAncillaryMessage::ScmRights(fds) = msg {
            for fd in fds {
                return Ok(fd);
            }
        }
    }

    Err(io::Error::new(
        io::ErrorKind::Other,
        "fusermount3 did not send /dev/fuse fd",
    ))
}

/// Unmount a FUSE filesystem via fusermount3 -u.
pub fn fuse_unmount(mountpoint: &Path) {
    let _ = Command::new("fusermount3")
        .args(["-u", "-q", "--"])
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

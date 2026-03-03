//! FUSE mount/unmount via fusermount3.
//!
//! Communicates with fusermount3 over a Unix socketpair, receiving the
//! `/dev/fuse` file descriptor via `SCM_RIGHTS`.

use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use rustix::io::FdFlags;
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SocketFlags, SocketType,
    recvmsg, socketpair,
};

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

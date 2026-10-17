//! FUSE mount/unmount.
//!
//! Preferred path: create a private user+mount namespace and call mount(2)
//! directly on /dev/fuse. No external helper, mount is invisible outside
//! our namespace, and the kernel tears it down automatically on exit.
//!
//! Fallback: the setuid helper protocol of `fusermount3` (socketpair +
//! SCM_RIGHTS) for systems where unprivileged user namespaces are disabled.
//! [`helper`] picks the helper.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
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
pub(crate) fn namespace_usable() -> bool {
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

/// The setuid helper that mounts for an unprivileged process: the one
/// `FUSERMOUNT_PROG` names, else `fusermount3`, else a `fusermount` that
/// is setuid root, each looked up on `PATH`.
///
/// `fusermount` from FUSE 2 speaks the same protocol as `fusermount3`. It
/// takes the socket from `_FUSE_COMMFD`, opens `/dev/fuse`, mounts, sends
/// the descriptor back with `SCM_RIGHTS`, and knows the same mount options
/// and `-u -z -q`. The kernel protocol is negotiated by the server once it
/// holds the descriptor, so the helper's major version does not matter.
/// Only a setuid one can mount for an ordinary user, and on some hosts
/// `fusermount` is a leftover without the bit.
pub fn helper() -> Option<PathBuf> {
    helper_in(
        std::env::var_os("FUSERMOUNT_PROG"),
        std::env::var_os("PATH"),
        rustix::process::geteuid().is_root(),
    )
}

fn helper_in(prog: Option<OsString>, path: Option<OsString>, root: bool) -> Option<PathBuf> {
    if let Some(prog) = prog.filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(prog));
    }
    let path = path.unwrap_or_else(|| "/usr/bin:/bin".into());
    let find = |name: &str| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|p| p.is_file())
    };
    find("fusermount3").or_else(|| {
        find("fusermount").filter(|p| {
            root || std::fs::metadata(p).is_ok_and(|m| m.uid() == 0 && m.mode() & 0o4000 != 0)
        })
    })
}

/// Mount a FUSE filesystem through [`helper`] and return the /dev/fuse fd.
///
/// Create a socketpair, pass one end to the helper via `_FUSE_COMMFD`,
/// then receive the /dev/fuse fd via SCM_RIGHTS on the other end.
pub fn fuse_mount(mountpoint: &Path) -> io::Result<OwnedFd> {
    let helper = helper().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no fusermount3 or setuid fusermount",
        )
    })?;
    let (sock_parent, sock_child) = socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )
    .map_err(|e| io::Error::other(format!("socketpair: {e}")))?;

    let child_fd = sock_child.as_raw_fd();

    let status = unsafe {
        Command::new(&helper)
            .args([
                "-o",
                &format!("ro,nosuid,nodev,noatime,{FUSE_PERM_OPTS}"),
                "--",
            ])
            .arg(mountpoint)
            .env("_FUSE_COMMFD", child_fd.to_string())
            .pre_exec(move || {
                // Clear CLOEXEC so the helper inherits this fd
                let fd = BorrowedFd::borrow_raw(child_fd);
                let flags = rustix::io::fcntl_getfd(fd).map_err(io::Error::other)?;
                rustix::io::fcntl_setfd(fd, flags.difference(FdFlags::CLOEXEC))
                    .map_err(io::Error::other)?;
                Ok(())
            })
            .status()
    }
    .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", helper.display())))?;

    // Drop child end so recvmsg doesn't block forever if the helper failed
    drop(sock_child);

    if !status.success() {
        return Err(io::Error::other(format!(
            "{} exited with {status}",
            helper.display()
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

    Err(io::Error::other(format!(
        "{} did not send /dev/fuse fd",
        helper.display()
    )))
}

/// Unmount a FUSE filesystem via [`helper`] `-u`.
/// Uses lazy unmount (-z) to handle dead mounts where the FUSE daemon exited.
pub fn fuse_unmount(mountpoint: &Path) {
    let Some(helper) = helper() else {
        return;
    };
    let _ = Command::new(helper)
        .args(["-u", "-z", "-q", "--"])
        .arg(mountpoint)
        .status();
}

/// Whether a mount helper is available: `fusermount3`, or one of the
/// others [`helper`] accepts.
pub fn fusermount3_available() -> bool {
    helper().is_some()
}

#[cfg(test)]
mod helper_tests {
    use std::os::unix::fs::PermissionsExt;

    use super::helper_in;

    /// `fusermount` was never tried, so a host with only FUSE 2's helper
    /// had no mount helper at all. It is taken when nothing better is
    /// there and it can mount: setuid root, or a caller that is root.
    #[test]
    fn fusermount_is_the_fallback_when_it_can_mount() {
        let dir = std::env::temp_dir().join(format!("onelf-helper-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = Some(dir.clone().into_os_string());
        let legacy = dir.join("fusermount");
        std::fs::write(&legacy, "").unwrap();
        std::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            helper_in(None, path.clone(), false),
            None,
            "a fusermount without the setuid bit cannot mount for a user"
        );
        assert_eq!(helper_in(None, path.clone(), true), Some(legacy));

        let current = dir.join("fusermount3");
        std::fs::write(&current, "").unwrap();
        assert_eq!(helper_in(None, path.clone(), false), Some(current));

        assert_eq!(
            helper_in(Some("/opt/fuse/fusermount".into()), path, false),
            Some("/opt/fuse/fusermount".into()),
            "FUSERMOUNT_PROG names the helper"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
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

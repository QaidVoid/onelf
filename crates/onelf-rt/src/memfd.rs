//! Anonymous in-memory execution via `memfd_create`.
//!
//! For small, self-contained entrypoints (no shared library dependencies),
//! the payload is decompressed directly into a memfd and executed without
//! touching the filesystem.

use std::io::{self, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::Command;

use rustix::fs::MemfdFlags;

pub fn execute_memfd(data: &[u8], argv0: &str, args: &[String]) -> io::Result<()> {
    let fd = rustix::fs::memfd_create(c"onelf", MemfdFlags::empty())
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("memfd_create: {e}")))?;

    let mut file = std::fs::File::from(fd);
    file.write_all(data)?;

    let raw_fd = file.as_raw_fd();
    let fd_path = format!("/proc/self/fd/{raw_fd}");

    let err = Command::new(&fd_path).arg0(argv0).args(args).exec();

    Err(io::Error::new(
        io::ErrorKind::Other,
        format!("exec failed: {err}"),
    ))
}

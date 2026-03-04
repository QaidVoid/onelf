//! FUSE-based execution mode.
//!
//! Mounts the package contents as a read-only FUSE filesystem and executes
//! the entrypoint directly from the mount. The parent process serves FUSE
//! requests while the child runs the target binary. A death pipe detects
//! child exit for reliable cleanup.

pub(crate) mod fs;
mod mount;
mod protocol;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};

use rustix::io::FdFlags;
use rustix::process::{Pid, Signal, WaitOptions, kill_process, waitpid};
use rustix::runtime::{KernelSigSet, KernelSigaction, KernelSigactionFlags, kernel_sigaction};

use crate::loader::PackageData;

static CHILD_PID: AtomicI32 = AtomicI32::new(0);

// x86_64 signal restorer -- calls rt_sigreturn (syscall 15).
// Required because kernel_sigaction bypasses libc, and x86_64 Linux
// requires SA_RESTORER for signal handler return to work.
#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".global __onelf_signal_restorer",
    ".type __onelf_signal_restorer, @function",
    "__onelf_signal_restorer:",
    "mov rax, 15",
    "syscall",
);

unsafe extern "C" {
    fn __onelf_signal_restorer();
}

unsafe extern "C" fn signal_handler(sig: core::ffi::c_int) {
    if sig == 17 {
        return; // SIGCHLD -- nothing to do, pipe detects child exit
    }
    // Forward other signals to child
    let pid = CHILD_PID.load(Ordering::Relaxed);
    if pid > 0 {
        if let Some(pid) = Pid::from_raw(pid) {
            if let Some(signal) = Signal::from_named_raw(sig) {
                let _ = kill_process(pid, signal);
            }
        }
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

fn create_mountpoint(package_name: &str, package_id: &[u8; 32]) -> Option<PathBuf> {
    let name_prefix: String = package_name.chars().take(6).collect();
    let hash_suffix = &package_id[0..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let dir_name = format!("onelf-{name_prefix}-{hash_suffix}");

    let base = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));

    let mountpoint = base.join(&dir_name);
    std::fs::create_dir_all(&mountpoint).ok()?;
    Some(mountpoint)
}

fn cleanup_mountpoint(mountpoint: &Path) {
    mount::fuse_unmount(mountpoint);
    let _ = std::fs::remove_dir(mountpoint);
}

/// Execute the package via FUSE mount.
///
/// On success, exits the process with the child's exit code (never returns).
/// Returns `false` if FUSE is unavailable and caller should fall back.
pub fn execute_fuse(
    pkg: &mut PackageData,
    ep_idx: usize,
    argv0: &str,
    exec_path: &str,
    args: &[String],
) -> bool {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    if !mount::fusermount3_available() {
        return false;
    }

    let mountpoint = match create_mountpoint(pkg.manifest.name(), &pkg.manifest.header.package_id) {
        Some(m) => m,
        None => return false,
    };

    let fuse_fd = match mount::fuse_mount(&mountpoint) {
        Ok(fd) => {
            // Set CLOEXEC so child doesn't inherit the FUSE fd after exec.
            let _ = rustix::io::fcntl_setfd(&fd, FdFlags::CLOEXEC);
            fd
        }
        Err(_) => {
            let _ = std::fs::remove_dir(&mountpoint);
            return false;
        }
    };

    // Resolve entrypoint target path
    let ep_target_entry = pkg.manifest.entrypoints[ep_idx].target_entry as usize;
    let ep_working_dir = pkg.manifest.entrypoints[ep_idx].working_dir;
    let ep_name = pkg
        .manifest
        .get_string(pkg.manifest.entrypoints[ep_idx].name)
        .to_string();
    let target_path_str = pkg.manifest.entry_path(ep_target_entry);
    let target_path = mountpoint.join(&target_path_str);

    let mountpoint_str = mountpoint.to_str().unwrap_or("").to_string();
    let lib_paths_str = pkg.manifest.lib_dirs().join(":");
    crate::env::setup_env(
        &mountpoint_str,
        argv0,
        exec_path,
        &ep_name,
        "fuse",
        &lib_paths_str,
    );

    // Set up portable directories
    let exe_path = std::path::Path::new(exec_path);
    let exe_dir = exe_path.parent().unwrap_or(std::path::Path::new("."));
    let exe_name = exe_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("onelf");
    crate::portable::setup_portable(exe_dir, exe_name);

    // Handle working directory
    let child_cwd: Option<PathBuf> = match ep_working_dir {
        onelf_format::WorkingDir::PackageRoot => Some(mountpoint.clone()),
        onelf_format::WorkingDir::EntrypointParent => target_path.parent().map(|p| p.to_path_buf()),
        onelf_format::WorkingDir::Inherit => None,
    };

    // Death pipe: when the child (and all its descendants) exit, the write end
    // closes and poll() on the read end returns POLLHUP.
    let (pipe_read, pipe_write) = match rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC) {
        Ok(p) => p,
        Err(_) => {
            cleanup_mountpoint(&mountpoint);
            return false;
        }
    };
    // Remove CLOEXEC from write end so the exec'd child inherits it.
    let _ = rustix::io::fcntl_setfd(&pipe_write, FdFlags::empty());

    use rustix::runtime::{Fork, kernel_fork};

    match unsafe { kernel_fork() } {
        Ok(Fork::Child(_)) => {
            let mut cmd = Command::new(&target_path);
            cmd.arg0(argv0).args(args);
            if let Some(cwd) = &child_cwd {
                cmd.current_dir(cwd);
            }
            let err = cmd.exec();
            eprintln!("onelf-rt: exec failed: {err}");
            std::process::exit(1);
        }
        Ok(Fork::ParentOf(child_pid)) => {
            // Close write end in parent -- only child holds it now
            drop(pipe_write);

            CHILD_PID.store(child_pid.as_raw_nonzero().get() as i32, Ordering::Relaxed);
            install_signal_handlers();

            let mut state = fs::FuseState::new(
                &pkg.manifest,
                &mut pkg.file,
                pkg.footer.payload_offset,
                pkg.dict.as_deref(),
            );

            let mut fuse_buf = vec![0u8; 1024 * 1024 + 4096];
            state.run_loop(&fuse_fd, &pipe_read, &mut fuse_buf);

            // Event loop exited -- reap child
            let exit_status = loop {
                match waitpid(Some(child_pid), WaitOptions::NOHANG) {
                    Ok(Some((_pid, status))) => break status,
                    Ok(None) => match waitpid(Some(child_pid), WaitOptions::empty()) {
                        Ok(Some((_pid, status))) => break status,
                        Ok(None) => continue,
                        Err(rustix::io::Errno::INTR) => continue,
                        Err(_) => {
                            cleanup_mountpoint(&mountpoint);
                            std::process::exit(1);
                        }
                    },
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(_) => {
                        cleanup_mountpoint(&mountpoint);
                        std::process::exit(1);
                    }
                }
            };

            drop(fuse_fd);
            cleanup_mountpoint(&mountpoint);

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
            cleanup_mountpoint(&mountpoint);
            eprintln!("onelf-rt: fork failed: {e}");
            std::process::exit(1);
        }
    }
}

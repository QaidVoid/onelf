//! FUSE-based execution mode.
//!
//! Mounts the package contents as a read-only FUSE filesystem and executes
//! the entrypoint directly from the mount. The parent process serves FUSE
//! requests while the child runs the target binary. A death pipe detects
//! child exit for reliable cleanup.

pub(crate) mod fs;
pub(crate) mod mount;
mod protocol;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};

use rustix::io::FdFlags;
use rustix::process::{Pid, Signal, WaitOptions, kill_process, waitpid};
use rustix::runtime::{KernelSigSet, KernelSigaction, KernelSigactionFlags, kernel_sigaction};

use crate::loader::PackageData;

static CHILD_PID: AtomicI32 = AtomicI32::new(0);

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

    if let Err(e) = std::fs::create_dir_all(&mountpoint) {
        eprintln!(
            "onelf-rt: fuse: cannot create {}: {e}",
            mountpoint.display()
        );
        return None;
    }
    Some(mountpoint)
}

/// Check if a path is currently a mountpoint by reading /proc/self/mountinfo.
/// This avoids stat/exists calls that can hang on dead FUSE mounts.
fn is_mountpoint(path: &Path) -> bool {
    let target = path.to_string_lossy();
    let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    // Field 5 (0-indexed: 4) in mountinfo is the mount point
    info.lines().any(|line| {
        line.split(' ')
            .nth(4)
            .map(|mp| mp.replace("\\040", " ") == *target)
            .unwrap_or(false)
    })
}

/// Execute directly from an existing FUSE mount (another instance is serving).
/// This process becomes the child — no fork/FUSE loop needed.
fn exec_from_mount(
    pkg: &mut PackageData,
    ep_idx: usize,
    argv0: &str,
    exec_path: &str,
    args: &[String],
    interp_data: Option<&[u8]>,
    env_data: Option<&[u8]>,
    mountpoint: &Path,
) -> bool {
    use std::os::unix::process::CommandExt;

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

    let exe_path = std::path::Path::new(exec_path);
    let exe_dir = exe_path.parent().unwrap_or(std::path::Path::new("."));
    let exe_name = exe_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("onelf");
    crate::portable::setup_portable(exe_dir, exe_name);

    let child_cwd: Option<PathBuf> = match ep_working_dir {
        onelf_format::WorkingDir::PackageRoot => Some(mountpoint.to_path_buf()),
        onelf_format::WorkingDir::EntrypointParent => target_path.parent().map(|p| p.to_path_buf()),
        onelf_format::WorkingDir::Inherit => None,
    };

    let target_path_s = target_path.to_str().unwrap_or("");
    let lib_path = crate::env::setup_env(
        &mountpoint_str,
        argv0,
        exec_path,
        &ep_name,
        "fuse",
        &lib_paths_str,
        target_path_s,
    );
    if let Some(data) = env_data {
        crate::env::apply_custom_env(data, &mountpoint_str);
    }

    let lib_dirs = pkg.manifest.lib_dirs();
    let bundled_interp_rel = interp_data.and_then(crate::interp::parse_bundled_interp_rel);

    if let Some(interp) =
        crate::interp::should_use_userland_exec(&target_path, mountpoint, bundled_interp_rel)
    {
        if let Some(cwd) = &child_cwd {
            let _ = std::env::set_current_dir(cwd);
        }
        crate::interp::exec_userland(&target_path, &interp, &lib_path, argv0, args);
    }

    let mut cmd = crate::interp::build_exec_command(
        &target_path,
        mountpoint,
        &lib_dirs,
        &lib_path,
        argv0,
        args,
    );
    if let Some(cwd) = &child_cwd {
        cmd.current_dir(cwd);
    }
    let err = cmd.exec();
    eprintln!("onelf-rt: exec failed: {err}");
    std::process::exit(1);
}

fn cleanup_mountpoint(mountpoint: &Path, used_namespace: bool) {
    if used_namespace {
        mount::fuse_unmount_direct(mountpoint);
    } else {
        mount::fuse_unmount(mountpoint);
    }
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
    interp_data: Option<&[u8]>,
    env_data: Option<&[u8]>,
) -> bool {
    use std::os::unix::process::CommandExt;

    // Tidy up empty mountpoint dirs left behind by previous runs.
    mount::sweep_stale_mountpoints();

    let mountpoint = match create_mountpoint(pkg.manifest.name(), &pkg.manifest.header.package_id) {
        Some(m) => m,
        None => return false,
    };

    // If already mounted by another instance, reuse it — just exec directly.
    // (Only reachable via the fusermount3 path; namespace mounts are private.)
    if is_mountpoint(&mountpoint) {
        if mountpoint.read_dir().is_ok() {
            return exec_from_mount(
                pkg,
                ep_idx,
                argv0,
                exec_path,
                args,
                interp_data,
                env_data,
                &mountpoint,
            );
        }
        // Dead mount (FUSE daemon exited). Clean up and proceed with fresh mount.
        mount::fuse_unmount(&mountpoint);
    }

    // Prefer the namespace-based mount. No external helper, private to us,
    // tears down automatically on exit. Fall back to fusermount3 if the
    // kernel disallows unprivileged user namespaces (e.g. restricted distros).
    //
    // Setting `ONELF_FUSE_NO_NAMESPACE=1` forces the fusermount3 path,
    // which is needed for packages that expect to stay in the host's
    // user namespace. The specific use case is rootless podman /
    // distrobox: they rely on setuid `newuidmap` / `newgidmap` to
    // build their own nested user namespace, and setuid bits do not
    // survive a CLONE_NEWUSER unshare. Staying in the host userns
    // keeps those helpers working.
    let skip_namespace = std::env::var_os("ONELF_FUSE_NO_NAMESPACE")
        .map(|v| v != "0" && !v.is_empty())
        .unwrap_or(false);

    let ns_result = if skip_namespace {
        Err(std::io::Error::other(
            "ONELF_FUSE_NO_NAMESPACE set; using fusermount3",
        ))
    } else {
        mount::fuse_mount_unshare(&mountpoint)
    };

    let (fuse_fd, used_namespace) = match ns_result {
        Ok(fd) => (fd, true),
        Err(ns_err) => {
            if !mount::fusermount3_available() {
                eprintln!("onelf-rt: fuse: namespace mount failed: {ns_err}");
                eprintln!("onelf-rt: fuse: fusermount3 not available either; cannot continue");
                let _ = std::fs::remove_dir(&mountpoint);
                return false;
            }
            match mount::fuse_mount(&mountpoint) {
                Ok(fd) => (fd, false),
                Err(e) => {
                    eprintln!("onelf-rt: fuse: mount failed: {e}");
                    let _ = std::fs::remove_dir(&mountpoint);
                    return false;
                }
            }
        }
    };
    // Set CLOEXEC so child doesn't inherit the FUSE fd after exec.
    let _ = rustix::io::fcntl_setfd(&fuse_fd, FdFlags::CLOEXEC);

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

    // Set up portable directories (doesn't access FUSE mount)
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

    // Extract bundled interpreter path for direct invocation (no symlinks needed)
    let bundled_interp_rel = interp_data.and_then(crate::interp::parse_bundled_interp_rel);

    // Death pipe: when the child (and all its descendants) exit, the write end
    // closes and poll() on the read end returns POLLHUP.
    let (pipe_read, pipe_write) = match rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC) {
        Ok(p) => p,
        Err(_) => {
            cleanup_mountpoint(&mountpoint, used_namespace);
            return false;
        }
    };
    // Remove CLOEXEC from write end so the exec'd child inherits it.
    let _ = rustix::io::fcntl_setfd(&pipe_write, FdFlags::empty());

    use rustix::runtime::{Fork, kernel_fork};

    match unsafe { kernel_fork() } {
        Ok(Fork::Child(_)) => {
            // setup_env must run in the child (after fork) because it probes
            // directories on the FUSE mount (lib/dri/, share/vulkan/, etc.).
            // The parent's FUSE event loop is now running concurrently.
            let target_path_s = target_path.to_str().unwrap_or("");
            let lib_path = crate::env::setup_env(
                &mountpoint_str,
                argv0,
                exec_path,
                &ep_name,
                "fuse",
                &lib_paths_str,
                target_path_s,
            );
            if let Some(data) = env_data {
                crate::env::apply_custom_env(data, &mountpoint_str);
            }

            let lib_dirs = pkg.manifest.lib_dirs();

            if let Some(interp) = crate::interp::should_use_userland_exec(
                &target_path,
                &mountpoint,
                bundled_interp_rel,
            ) {
                if let Some(cwd) = &child_cwd {
                    let _ = std::env::set_current_dir(cwd);
                }
                crate::interp::exec_userland(&target_path, &interp, &lib_path, argv0, args);
            }

            let mut cmd = crate::interp::build_exec_command(
                &target_path,
                &mountpoint,
                &lib_dirs,
                &lib_path,
                argv0,
                args,
            );
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
                            cleanup_mountpoint(&mountpoint, used_namespace);
                            std::process::exit(1);
                        }
                    },
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(_) => {
                        cleanup_mountpoint(&mountpoint, used_namespace);
                        std::process::exit(1);
                    }
                }
            };

            drop(fuse_fd);
            cleanup_mountpoint(&mountpoint, used_namespace);

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
            cleanup_mountpoint(&mountpoint, used_namespace);
            eprintln!("onelf-rt: fork failed: {e}");
            std::process::exit(1);
        }
    }
}

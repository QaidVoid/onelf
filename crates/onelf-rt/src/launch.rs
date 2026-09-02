//! The last step of every execution mode: decide what the host supplies,
//! set up the environment, and replace this process with the entrypoint.
//!
//! FUSE, tmpfs and cache modes differ in how the package tree comes to
//! exist on disk and in nothing after that, so they share this one path.
//! The memfd mode has no tree and no libraries and does not come here.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use onelf_format::HostLibsPolicy;
use onelf_format::drivers;
use onelf_format::resolve::{self, Gl, Request, Resolution};

use crate::loader::PackageData;

/// Everything a launch needs beyond the package itself.
pub struct Launch<'a> {
    pub pkg: &'a PackageData,
    /// The package root as the entrypoint will see it: the mount, the
    /// tmpfs, or the extracted directory.
    pub pkg_root: &'a Path,
    /// Reported to the app as `ONELF_ACTIVE_MODE`.
    pub mode: &'static str,
    pub ep_idx: usize,
    pub argv0: &'a str,
    pub exec_path: &'a str,
    pub args: &'a [String],
    pub interp_data: Option<&'a [u8]>,
    pub env_data: Option<&'a [u8]>,
    /// Whether this process is inside a private mount namespace, which
    /// decides how a self-extracting binary gets its loader and how a host
    /// library shadows its bundled copy.
    pub private_ns: bool,
    /// Whether the package tree belongs to this launch alone and may be
    /// edited in place, the other way a host library can shadow a bundled
    /// copy when there is no namespace to mount in.
    pub tree_writable: bool,
}

/// Replace this process with the entrypoint. Only returns by exiting.
pub fn exec(launch: &Launch) -> ! {
    let manifest = &launch.pkg.manifest;
    let ep = &manifest.entrypoints[launch.ep_idx];
    let ep_name = manifest.get_string(ep.name).to_string();
    let target_path = launch
        .pkg_root
        .join(manifest.entry_path(ep.target_entry as usize));
    let target_path_s = target_path.to_str().unwrap_or("");
    let pkg_root_s = launch.pkg_root.to_str().unwrap_or("");
    let lib_dirs = manifest.lib_dirs();
    let lib_paths_str = lib_dirs.join(":");

    let target_is_elf = crate::env::is_elf_file(target_path_s);
    let resolved = if target_is_elf && !lib_dirs.is_empty() {
        resolve_for(launch.pkg, launch.pkg_root, &lib_dirs)
    } else {
        Resolved::default()
    };
    let resolution = &resolved.resolution;
    for name in &resolution.incomparable {
        eprintln!("onelf-rt: resolver: cannot order {name}; keeping the bundled copy");
    }
    if let Some(farm) = &resolution.farm {
        shadow_bundled_copies(farm, launch, &lib_dirs);
    }

    let lib_path = crate::env::setup_env(
        pkg_root_s,
        launch.argv0,
        launch.exec_path,
        &ep_name,
        launch.mode,
        &lib_paths_str,
        target_path_s,
        resolution.farm.as_deref(),
        resolved.platform_root.as_deref(),
    );
    if let Some(data) = launch.env_data {
        crate::env::apply_custom_env(data, pkg_root_s);
    }

    match ep.working_dir {
        onelf_format::WorkingDir::PackageRoot => {
            let _ = std::env::set_current_dir(launch.pkg_root);
        }
        onelf_format::WorkingDir::EntrypointParent => {
            if let Some(parent) = target_path.parent() {
                let _ = std::env::set_current_dir(parent);
            }
        }
        onelf_format::WorkingDir::Inherit => {}
    }

    let bundled_interp_rel = launch
        .interp_data
        .and_then(crate::interp::parse_bundled_interp_rel);

    if let Some(host_interp) = &resolution.host_interp {
        exec_under_host_loader(&target_path, host_interp, &lib_path, launch);
    }

    if let Some(interp) =
        crate::interp::should_use_userland_exec(&target_path, launch.pkg_root, bundled_interp_rel)
    {
        crate::interp::exec_userland(&target_path, &interp, &lib_path, launch.argv0, launch.args);
    }

    let mut cmd = crate::interp::build_exec_command(
        &target_path,
        launch.pkg_root,
        &lib_dirs,
        &lib_path,
        launch.private_ns,
        launch.argv0,
        launch.args,
    );
    let err = cmd.exec();
    eprintln!("onelf-rt: exec failed: {err}");
    std::process::exit(1);
}

/// Put each host winner in place of the bundled copy it beat.
///
/// The farm on the library path is not enough for that: `bundle-libs`
/// gives every executable a `DT_RPATH`, and the loader searches that
/// before any library path, so a bundled copy would always be found
/// first. Inside a private mount namespace the host file is bind-mounted
/// over the bundled path, which every process in the tree then sees,
/// including one that clears its environment and re-execs. A tree that is
/// this launch's own gets a symlink instead. With neither, the farm is
/// all there is, and only libraries the bundle does not carry can come
/// from the host.
fn shadow_bundled_copies(farm: &Path, launch: &Launch, lib_dirs: &[&str]) {
    if !launch.private_ns && !launch.tree_writable {
        return;
    }
    let Ok(entries) = std::fs::read_dir(farm) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(host) = std::fs::read_link(entry.path()) else {
            continue;
        };
        for dir in lib_dirs {
            let bundled = launch.pkg_root.join(dir).join(entry.file_name());
            if !bundled.is_file() {
                continue;
            }
            let outcome = if launch.private_ns {
                rustix::mount::mount_bind(&host, &bundled).map_err(std::io::Error::from)
            } else {
                std::fs::remove_file(&bundled)
                    .and_then(|()| std::os::unix::fs::symlink(&host, &bundled))
            };
            if let Err(e) = outcome {
                eprintln!(
                    "onelf-rt: resolver: cannot place {} over {}: {e}",
                    host.display(),
                    bundled.display()
                );
            }
        }
    }
}

/// The host's libc won, so the entrypoint runs under the host loader with
/// the link farm shadowing every bundled glibc file. The bundled loader
/// must not run at all here, or it would load a libc that does not match
/// the one the farm now names.
///
/// A bootstrap-injected binary carries no `PT_INTERP` and maps its loader
/// itself, so it is kernel-exec'd with `ONELF_INTERP` naming the host
/// loader; the bootstrap honours that over its own relative path, and so
/// does every re-exec the app performs. Anything else runs under the host
/// loader directly: userland exec keeps `/proc/self/exe` pointing at the
/// entrypoint, and a non-PIE binary, or one that carries a trailer the
/// loader path cannot preserve, is handed to the loader on its command
/// line instead.
fn exec_under_host_loader(target: &Path, host_interp: &Path, lib_path: &str, launch: &Launch) {
    let interp = crate::interp::read_elf_interp(target);
    // A self-extracting binary needs `/proc/self/exe` to be itself, which
    // only a kernel exec gives it. Its `PT_INTERP` names the host loader,
    // and the host loader is what won, so the kernel can be left to it.
    let kernel_exec = match &interp {
        None => true,
        Some(interp) => {
            crate::selfextract::has_self_extract_trailer(target) && Path::new(interp).is_file()
        }
    };
    if kernel_exec {
        let mut cmd = Command::new(target);
        cmd.arg0(launch.argv0)
            .args(launch.args)
            .env("ONELF_INTERP", host_interp);
        let host_only: Vec<&str> = lib_path
            .split(':')
            .filter(|dir| !dir.is_empty() && !Path::new(dir).starts_with(launch.pkg_root))
            .collect();
        if !host_only.is_empty() {
            cmd.env("LD_LIBRARY_PATH", host_only.join(":"));
        }
        let err = cmd.exec();
        eprintln!("onelf-rt: exec failed: {err}");
        std::process::exit(1);
    }

    let direct = crate::ulexec::is_supported()
        && crate::interp::is_pie(target)
        && !crate::selfextract::has_self_extract_trailer(target);
    if direct {
        crate::interp::exec_userland(target, host_interp, lib_path, launch.argv0, launch.args);
    }
    let mut cmd = Command::new(host_interp);
    cmd.arg("--inhibit-cache");
    if !lib_path.is_empty() {
        cmd.arg("--library-path").arg(lib_path);
    }
    cmd.arg("--argv0")
        .arg(launch.argv0)
        .arg(target)
        .args(launch.args);
    let err = cmd.exec();
    eprintln!(
        "onelf-rt: exec under {} failed: {err}",
        host_interp.display()
    );
    std::process::exit(1);
}

/// What the resolver decided, with the GL build it was given, if any.
#[derive(Default)]
struct Resolved {
    resolution: Resolution,
    /// The fetched GL build's extracted root.
    platform_root: Option<std::path::PathBuf>,
    /// Held through exec so the extraction is not collected while this
    /// instance runs. The fd is inheritable.
    _platform_lock: Option<std::fs::File>,
}

/// Run the resolver for this package on this host. `ONELF_NO_RESOLVER`
/// turns it off for one launch, which is the way to tell whether a
/// failure is the resolver's doing.
///
/// A host with no GL stack gets the build the package pins, if it pins
/// one and the build can be had; the resolver then indexes that build
/// ahead of the host. Anything that stops that is a warning, and the
/// launch goes on without a GL stack.
fn resolve_for(pkg: &PackageData, pkg_root: &Path, lib_dirs: &[&str]) -> Resolved {
    let disabled = std::env::var_os("ONELF_NO_RESOLVER").is_some_and(|v| !v.is_empty() && v != "0");
    let policy = if disabled {
        HostLibsPolicy::Never
    } else {
        HostLibsPolicy::from_flags(pkg.footer.flags)
    };
    if policy == HostLibsPolicy::Never {
        return Resolved::default();
    }
    let Some(store) = crate::paths::resolve_store(&pkg.manifest.header.package_id) else {
        return Resolved::default();
    };
    let ld_cache = drivers::cache_file();

    let mut platform = None;
    if resolve::gl_situation(pkg_root, lib_dirs, &ld_cache, resolve::ICD_DIRS) == Gl::Absent {
        match crate::platform::obtain(pkg_root) {
            Ok(fetched) => platform = Some(fetched),
            Err(why) => {
                eprintln!("onelf-rt: this host has no GL stack and {why}; continuing without one")
            }
        }
    }
    let (platform_root, lock) = match platform {
        Some((root, lock)) => (Some(root), Some(lock)),
        None => (None, None),
    };

    let resolution = resolve::resolve(&Request {
        pkg_root,
        lib_dirs,
        policy,
        store: &store,
        ld_cache: &ld_cache,
        icd_dirs: resolve::ICD_DIRS,
        extra_root: platform_root.as_deref(),
    });
    Resolved {
        resolution,
        platform_root,
        _platform_lock: lock,
    }
}

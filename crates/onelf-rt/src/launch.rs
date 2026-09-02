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

use crate::loader::PackageData;
use crate::resolve::{self, Request, Resolution};

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
    /// decides how a self-extracting binary gets its loader.
    pub private_ns: bool,
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
    let resolution = if target_is_elf && !lib_dirs.is_empty() {
        resolve_for(launch.pkg, launch.pkg_root, &lib_dirs)
    } else {
        Resolution::default()
    };
    for name in &resolution.incomparable {
        eprintln!("onelf-rt: resolver: cannot order {name}; keeping the bundled copy");
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

/// The host's libc won, so the entrypoint runs under the host loader with
/// the link farm shadowing every bundled glibc file. The bundled loader
/// must not run at all here: a bootstrap-injected binary would jump into
/// it and load a libc that does not match the one the farm now names.
///
/// Userland exec keeps `/proc/self/exe` pointing at the entrypoint. A
/// non-PIE binary, or one that carries a trailer the loader path cannot
/// preserve, is handed to the host loader on its command line instead.
fn exec_under_host_loader(target: &Path, host_interp: &Path, lib_path: &str, launch: &Launch) {
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

/// Run the resolver for this package on this host. `ONELF_NO_RESOLVER`
/// turns it off for one launch, which is the way to tell whether a
/// failure is the resolver's doing.
fn resolve_for(pkg: &PackageData, pkg_root: &Path, lib_dirs: &[&str]) -> Resolution {
    let disabled = std::env::var_os("ONELF_NO_RESOLVER").is_some_and(|v| !v.is_empty() && v != "0");
    let policy = if disabled {
        HostLibsPolicy::Never
    } else {
        HostLibsPolicy::from_flags(pkg.footer.flags)
    };
    if policy == HostLibsPolicy::Never {
        return Resolution::default();
    }
    let Some(store) = crate::paths::resolve_store(&pkg.manifest.header.package_id) else {
        return Resolution::default();
    };
    resolve::resolve(&Request {
        pkg_root,
        lib_dirs,
        policy,
        store: &store,
        ld_cache: &drivers::cache_file(),
        icd_dirs: resolve::ICD_DIRS,
    })
}

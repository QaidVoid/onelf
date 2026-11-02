//! The last step of every execution mode: decide what the host supplies,
//! set up the environment, and replace this process with the entrypoint.
//!
//! FUSE, tmpfs and cache modes differ in how the package tree comes to
//! exist on disk and in nothing after that, so they share this one path.
//! The memfd mode has no tree and no libraries and does not come here.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use onelf_format::HostLibsPolicy;
use onelf_format::drivers;
use onelf_format::resolve::{self, Gl, Request, Resolution};
use onelf_format::rules::Inheritance;

use crate::loader::PackageData;

/// Where the launch publishes the evaluated per-view environment input the
/// payload constructor reads back.
const VIEW_ENV_FILE: &str = ".onelf/runtime-env";
/// The generated input is capped at one mebibyte, the bound the
/// constructor enforces when it reads the file back; the two numbers must
/// stay equal.
const MAX_VIEW_ENV_BYTES: usize = 1024 * 1024;
/// The most entries the input may carry, as the constructor counts them.
const MAX_VIEW_ENV_ENTRIES: usize = 4096;
/// The longest variable name an entry may carry.
const MAX_VIEW_ENV_NAME: usize = 128;
/// The longest value an entry or scalar record may carry.
const MAX_VIEW_ENV_VALUE: usize = 32 * 1024;
const VIEW_ENV_HEADER: &[u8] = b"ONELF-RUNTIME-ENV 1 ";
const HOST_ENV_VAR: &str = "ONELF_HOST_ENV";
const ROOTS_VAR: &str = "ONELF_ROOTS";

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
    pub args: &'a [OsString],
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
    /// How the libraries the launch takes from outside the package come to
    /// show in its library directory.
    pub links: Links<'a>,
}

/// Where a launch's library links (see [`crate::links`]) are put.
pub enum Links<'a> {
    /// The tree is on disk and writable: the links are symlinks in it.
    Tree,
    /// The FUSE server this launch started is told, and serves them.
    Server(&'a crate::links::Sender),
    /// The mount is another launch's, which must serve these links already.
    Served,
}

/// Replace this process with the entrypoint. Only returns by exiting.
pub fn exec(launch: &Launch) -> ! {
    run(launch);
    eprintln!("onelf-rt: the library links of this launch could not be placed");
    std::process::exit(1);
}

/// [`exec`] on a mount another launch serves. Returns, before anything of
/// the environment is set up, when that mount serves other library links
/// than this launch needs, so the caller can choose another mode.
pub fn exec_served(launch: &Launch) {
    run(launch);
}

/// Replace this process with the entrypoint, unless its library links
/// cannot be placed, which only a mount served by another launch refuses.
fn run(launch: &Launch) {
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

    // The sets the package left its dependencies to, obtained before any
    // mode made a mount namespace of its own.
    let set_roots = crate::sets::roots();
    let set_lib_dirs: Vec<std::path::PathBuf> = set_roots
        .iter()
        .flat_map(|r| crate::sets::lib_dirs(r))
        .collect();

    // The resolver judges the package, not the entrypoint: a script
    // execs the bundled binaries too.
    let resolved = if lib_dirs.is_empty() {
        Resolved::default()
    } else {
        resolve_for(launch, &lib_dirs, &set_roots)
    };
    let resolution = &resolved.resolution;
    for name in &resolution.incomparable {
        eprintln!("onelf-rt: resolver: cannot order {name}; keeping the bundled copy");
    }
    if let Some(farm) = &resolution.farm {
        shadow_bundled_copies(farm, launch, &lib_dirs);
    }
    let runpath = crate::links::origin_runpath(&target_path);
    if !lib_dirs.is_empty()
        && !place_links(
            launch,
            &lib_dirs,
            &runpath,
            resolution.farm.as_deref(),
            &set_roots,
        )
    {
        return;
    }

    let env_before = snapshot_env();
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
        &set_lib_dirs,
        &set_roots,
    );
    if let Some(data) = launch.env_data {
        crate::env::apply_custom_env(data, pkg_root_s);
    }
    crate::hostenv::record_host_env(launch.pkg_root, &set_roots);
    publish_view_env(launch, &env_before, &lib_dirs, &set_lib_dirs, &set_roots);
    // Last, so the mount namespace this may enter holds nothing but the
    // exec that follows.
    crate::hostpaths::apply(launch.pkg_root);

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

    // Everything above read the mount for the runtime's own sake: the
    // resolver lists and reads every bundled library. A trace starts
    // here, so it holds what the app opens and not what onelf did.
    if launch.mode == "fuse" && std::env::var_os("ONELF_TRACE").is_some_and(|v| !v.is_empty()) {
        let _ = std::fs::symlink_metadata(launch.pkg_root.join(crate::fuse::TRACE_ARM));
    }

    // The app's argv[0] is the file that actually executes, not the pack
    // that carries it. A binary that locates its data files by resolving
    // argv[0] would otherwise look beside the pack, where nothing of the
    // app lives. ONELF_ARGV0 keeps the name the user launched. An
    // entrypoint named apart from its target keeps the launched name,
    // since a multicall binary dispatches on it.
    let app_argv0: &str = if target_path.file_name() == Some(ep_name.as_ref()) {
        target_path_s
    } else {
        launch.argv0
    };

    let bundled_interp_rel = launch
        .interp_data
        .and_then(crate::interp::parse_bundled_interp_rel);

    // The only environment a launch ever adds to the library search, and
    // only for an ELF that has no `$ORIGIN` run path and that the kernel
    // has to start: nothing else would lead it to its own libraries.
    let kernel_lib_path = (!lib_path.is_empty() && runpath.is_empty())
        .then(|| fallback_library_path(launch.pkg_root, &lib_dirs));

    if let Some(host_interp) = &resolution.host_interp {
        exec_under_host_loader(
            &target_path,
            host_interp,
            &lib_path,
            kernel_lib_path.as_deref(),
            app_argv0,
            launch,
        );
    }

    if let Some(interp) =
        crate::interp::should_use_userland_exec(&target_path, launch.pkg_root, bundled_interp_rel)
    {
        crate::interp::exec_userland(&target_path, &interp, &lib_path, app_argv0, launch.args);
    }

    let mut cmd = crate::interp::build_exec_command(
        &target_path,
        launch.pkg_root,
        &lib_dirs,
        &lib_path,
        kernel_lib_path.as_deref(),
        launch.private_ns,
        app_argv0,
        launch.args,
    );
    let err = cmd.exec();
    eprintln!("onelf-rt: exec failed: {err}");
    std::process::exit(1);
}

/// Show the libraries this launch takes from the farm and the sets in the
/// package's library directory. False when the mount another launch
/// serves shows a different set, which this launch cannot change.
fn place_links(
    launch: &Launch,
    lib_dirs: &[&str],
    runpath: &[std::path::PathBuf],
    farm: Option<&Path>,
    set_roots: &[std::path::PathBuf],
) -> bool {
    let manifest = &launch.pkg.manifest;
    let Some(dir) = crate::links::dir(manifest, launch.pkg_root, runpath, lib_dirs) else {
        return true;
    };
    let links = crate::links::wanted(manifest, &dir, farm, set_roots);
    match launch.links {
        Links::Tree => {
            crate::links::place(manifest, launch.pkg_root, &dir, &links);
            true
        }
        Links::Server(sender) => {
            if !links.is_empty() {
                sender.send(&dir, &links);
            }
            true
        }
        Links::Served => crate::links::served(manifest, launch.pkg_root, &dir, &links, lib_dirs),
    }
}

/// The environment this launch found, before it changed any of it.
fn snapshot_env() -> BTreeMap<OsString, OsString> {
    std::env::vars_os().collect()
}

/// One evaluated assignment encoded for the constructor.
struct ViewEnvEntry {
    name: Vec<u8>,
    value: Vec<u8>,
    policy: Inheritance,
    applied: Option<u64>,
}

/// Why a generated input was refused instead of written short.
#[derive(Debug)]
enum ViewEnvError {
    /// The whole input would exceed the bound the constructor reads back.
    TooLarge,
}

impl fmt::Display for ViewEnvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("beyond the record bound the constructor reads back")
    }
}

/// Whether this launch may write the per-view environment input into its
/// tree: the tree must belong to this launch alone, so a shared extracted
/// tree or a mount another launch serves is never written to. Views that
/// carry no file run on the packaged `.onelf/env` alone.
fn publishes_view_env(links: &Links<'_>, tree_writable: bool, private_ns: bool) -> bool {
    matches!(links, Links::Tree) && (tree_writable || private_ns)
}

/// The launch's framework mappings, evaluated against the application and
/// set roots the same pass `crate::env` applied them with, so the record
/// carries each mapping's value and inheritance policy.
fn view_env_assignments(
    pkg_root: &Path,
    lib_dirs: &[&str],
    set_lib_dirs: &[PathBuf],
    set_roots: &[PathBuf],
) -> Vec<crate::env::PathAssignment<'static>> {
    let app_lib_dirs: Vec<PathBuf> = lib_dirs.iter().map(|d| pkg_root.join(d)).collect();
    let set_packages: Vec<&Path> = set_roots.iter().map(PathBuf::as_path).collect();
    let packages = [pkg_root];
    let scopes = [
        crate::env::Scope {
            packages: &packages,
            libraries: &app_lib_dirs,
        },
        crate::env::Scope {
            packages: &set_packages,
            libraries: set_lib_dirs,
        },
    ];
    crate::env::evaluate_path_groups(crate::env::builtin_path_groups(), &scopes)
}

/// Generate and publish this launch's evaluated per-view environment input.
///
/// The record holds one entry per framework mapping alias and per variable
/// the launch changed, the active roots, and the host-restoration record.
/// The constructor reads it back after a sandboxed `clearenv`, restoring
/// the mappings the clearing took away and re-materializing the records
/// the exec hooks need. An input beyond the constructor's bounds is
/// refused whole rather than written short; the packaged `.onelf/env`
/// still covers the launch, as it does without a record.
fn publish_view_env(
    launch: &Launch,
    base: &BTreeMap<OsString, OsString>,
    lib_dirs: &[&str],
    set_lib_dirs: &[PathBuf],
    set_roots: &[PathBuf],
) {
    if !publishes_view_env(&launch.links, launch.tree_writable, launch.private_ns) {
        return;
    }
    let assignments = view_env_assignments(launch.pkg_root, lib_dirs, set_lib_dirs, set_roots);
    let now = snapshot_env();
    let roots = now.get(OsStr::new(ROOTS_VAR)).map(OsString::as_os_str);
    let host_record = now.get(OsStr::new(HOST_ENV_VAR)).map(OsString::as_os_str);
    match encode_view_env(base, &now, &assignments, roots, host_record) {
        Ok(bytes) if !bytes.is_empty() => {
            if let Err(e) = std::fs::write(launch.pkg_root.join(VIEW_ENV_FILE), &bytes) {
                eprintln!("onelf-rt: cannot write {VIEW_ENV_FILE}: {e}");
            }
        }
        Ok(_) => {}
        Err(e) => eprintln!("onelf-rt: the generated environment input is {e} and is not written"),
    }
}

/// Encode the evaluated per-view environment input.
///
/// Every framework mapping alias is listed with the mapping's own value,
/// what the launch left in the environment to compare against, and the
/// mapping's inheritance policy. Every other variable the launch changed
/// is listed with the value it left, under a replace policy. A variable
/// the launch left unset is not listed: restoration never invents an
/// unset. A name or value beyond what the constructor reads back is
/// skipped, since the packaged `.onelf/env` still carries recipe values.
fn encode_view_env(
    base: &BTreeMap<OsString, OsString>,
    now: &BTreeMap<OsString, OsString>,
    assignments: &[crate::env::PathAssignment<'_>],
    roots: Option<&OsStr>,
    host_record: Option<&OsStr>,
) -> Result<Vec<u8>, ViewEnvError> {
    let mut mapped = BTreeSet::new();
    let mut entries: Vec<ViewEnvEntry> = Vec::new();
    for assignment in assignments {
        for name in &assignment.variables {
            if !mapped.insert(OsString::from(*name)) {
                continue;
            }
            let applied = now
                .get(OsStr::new(name))
                .map(|left| crate::hostenv::fnv1a(left.as_bytes()));
            entries.push(ViewEnvEntry {
                name: name.as_bytes().to_vec(),
                value: assignment.value.as_bytes().to_vec(),
                policy: assignment.inheritance,
                applied,
            });
        }
    }
    for (name, value) in now {
        if base.get(name) == Some(value)
            || name.as_bytes() == HOST_ENV_VAR.as_bytes()
            || name.as_bytes() == ROOTS_VAR.as_bytes()
            || mapped.contains(name)
        {
            continue;
        }
        let (name, value) = (name.as_bytes(), value.as_bytes());
        if name.is_empty()
            || name.len() > MAX_VIEW_ENV_NAME
            || name.contains(&0)
            || name.contains(&b'=')
            || value.len() > MAX_VIEW_ENV_VALUE
            || value.contains(&0)
        {
            continue;
        }
        entries.push(ViewEnvEntry {
            name: name.to_vec(),
            value: value.to_vec(),
            policy: Inheritance::Replace,
            applied: Some(crate::hostenv::fnv1a(value)),
        });
    }
    if entries.len() > MAX_VIEW_ENV_ENTRIES {
        return Err(ViewEnvError::TooLarge);
    }

    let mut out = Vec::new();
    put(&mut out, VIEW_ENV_HEADER)?;
    put(&mut out, entries.len().to_string().as_bytes())?;
    put(&mut out, b"\n")?;
    for entry in &entries {
        put(&mut out, &entry.name)?;
        put(&mut out, b"=")?;
        put(
            &mut out,
            match entry.policy {
                Inheritance::Preserve => b".",
                Inheritance::Replace => b"!",
            },
        )?;
        put(&mut out, entry.value.len().to_string().as_bytes())?;
        put(&mut out, b":")?;
        put(&mut out, &entry.value)?;
        match entry.applied {
            Some(hash) => {
                put(&mut out, b"@")?;
                put(&mut out, format!("{hash:016x}").as_bytes())?;
            }
            None => put(&mut out, b"-")?,
        }
        put(&mut out, b";\n")?;
    }
    for (tag, scalar) in [(b'R', roots), (b'H', host_record)] {
        let Some(bytes) = scalar else {
            continue;
        };
        let bytes = bytes.as_bytes();
        if bytes.len() > MAX_VIEW_ENV_VALUE || bytes.contains(&0) {
            continue;
        }
        put(&mut out, &[tag])?;
        put(&mut out, bytes.len().to_string().as_bytes())?;
        put(&mut out, b":")?;
        put(&mut out, bytes)?;
        put(&mut out, b";\n")?;
    }
    Ok(out)
}

/// Append `bytes` while the whole input stays within the constructor's
/// bound. The first byte past it refuses the input whole.
fn put(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), ViewEnvError> {
    if out.len() + bytes.len() > MAX_VIEW_ENV_BYTES {
        return Err(ViewEnvError::TooLarge);
    }
    out.extend_from_slice(bytes);
    Ok(())
}

/// `LD_LIBRARY_PATH` for an ELF without a `$ORIGIN` run path that the
/// kernel starts: the package's library directories, which hold the
/// library links as well, ahead of the caller's own path.
///
/// Such a binary is one the packer gave no run path: a self-extracting
/// one, which needs the kernel to make `/proc/self/exe` itself, one
/// bundled where neither a slot nor patchelf was there to write one, or
/// one packed without `bundle-libs` that runs under the host's loader. A
/// bootstrapped binary maps its loader itself, so no loader of ours could
/// be handed `--library-path` for it, and a host loader may predate the
/// `--argv0` the command line would need. This is the one place the
/// variable is still set, and what the app spawns inherits it.
fn fallback_library_path(pkg_root: &Path, lib_dirs: &[&str]) -> String {
    let mut parts: Vec<String> = lib_dirs
        .iter()
        .map(|d| pkg_root.join(d).to_string_lossy().into_owned())
        .collect();
    if let Some(existing) = std::env::var_os("LD_LIBRARY_PATH").filter(|v| !v.is_empty()) {
        parts.push(existing.to_string_lossy().into_owned());
    }
    parts.join(":")
}

/// Put each host winner in place of the bundled copy it beat.
///
/// A link beside the bundled copy is not enough for that, and neither is
/// a library path: `bundle-libs` gives every executable a `DT_RPATH`, and
/// the loader searches that before any library path, so a bundled copy
/// would always be found first. Inside a private mount namespace the host
/// file is bind-mounted over the bundled path, which every process in the
/// tree then sees, including one that clears its environment and
/// re-execs. A tree that is this launch's own gets a symlink instead.
/// With neither, only libraries the bundle does not carry can come from
/// the host.
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
/// loader directly: userland exec never updates `/proc/self/exe`, so it
/// keeps naming the packed file rather than the entrypoint, which ordinary
/// binaries do not mind, and a non-PIE binary, or one that carries a
/// trailer the loader path cannot preserve, is handed to the loader on its
/// command line instead.
fn exec_under_host_loader(
    target: &Path,
    host_interp: &Path,
    lib_path: &str,
    kernel_lib_path: Option<&str>,
    argv0: &str,
    launch: &Launch,
) {
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
        cmd.arg0(argv0)
            .args(launch.args)
            .env("ONELF_INTERP", host_interp);
        if let Some(path) = kernel_lib_path {
            cmd.env("LD_LIBRARY_PATH", path);
        }
        let err = cmd.exec();
        eprintln!("onelf-rt: exec failed: {err}");
        std::process::exit(1);
    }

    let direct = crate::ulexec::is_supported()
        && crate::interp::is_pie(target)
        && !crate::selfextract::has_self_extract_trailer(target);
    if direct {
        crate::interp::exec_userland(target, host_interp, lib_path, argv0, launch.args);
    }
    let mut cmd = Command::new(host_interp);
    cmd.arg("--inhibit-cache");
    if !lib_path.is_empty() {
        cmd.arg("--library-path").arg(lib_path);
    }
    cmd.arg("--argv0").arg(argv0).arg(target).args(launch.args);
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
/// A launch that can put no host file in place of a bundled one, neither
/// by mount nor by editing its tree, never takes the host's glibc: the
/// bundled executables would load their own libc under the host loader.
///
/// A host with no GL stack gets the build the package pins, if it pins
/// one and the build can be had; the resolver then indexes that build
/// ahead of the host. Anything that stops that is a warning, and the
/// launch goes on without a GL stack.
fn resolve_for(launch: &Launch, lib_dirs: &[&str], set_roots: &[std::path::PathBuf]) -> Resolved {
    let (pkg, pkg_root) = (launch.pkg, launch.pkg_root);
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
    // A set holds the toolkit, and the toolkit is what usually names a
    // GL entry point, so its directories count towards the decision.
    let set_lib_dirs: Vec<std::path::PathBuf> = set_roots
        .iter()
        .flat_map(|root| crate::sets::lib_dirs(root))
        .collect();
    let ep = &pkg.manifest.entrypoints[launch.ep_idx];
    let executable = pkg_root.join(pkg.manifest.entry_path(ep.target_entry as usize));
    let gl = resolve::gl_situation(
        pkg_root,
        lib_dirs,
        &set_lib_dirs,
        Some(&executable),
        &ld_cache,
        resolve::ICD_DIRS,
    );
    if gl.lacking() {
        // Without a pin, a package that links no GL entry point has
        // nothing to warn about: it only ever opens one by name, when
        // the application draws with GL.
        match crate::platform::obtain(pkg_root) {
            Some(Ok(fetched)) => platform = Some(fetched),
            Some(Err(why)) => eprintln!(
                "onelf-rt: this host has no GL stack and the pinned build cannot be used \
                 ({why}); continuing without one"
            ),
            None if gl == Gl::Missing => eprintln!(
                "onelf-rt: this host has no GL stack, which the package links, and the \
                 package pins no GL build; continuing without one"
            ),
            None => {}
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
        set_roots,
        host_libc: launch.private_ns || launch.tree_writable,
    });
    Resolved {
        resolution,
        platform_root,
        _platform_lock: lock,
    }
}

#[cfg(test)]
mod view_env_tests {
    use super::{
        HOST_ENV_VAR, ROOTS_VAR, VIEW_ENV_HEADER, encode_view_env, publishes_view_env, snapshot_env,
    };
    use onelf_format::rules::Inheritance;
    use std::collections::BTreeMap;
    use std::ffi::{OsStr, OsString};

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<OsString, OsString> {
        pairs
            .iter()
            .map(|(name, value)| (OsString::from(*name), OsString::from(*value)))
            .collect()
    }

    fn assignments<'a>(
        items: &[(&'a str, &'a str, Inheritance)],
    ) -> Vec<crate::env::PathAssignment<'a>> {
        items
            .iter()
            .map(|(name, value, inheritance)| crate::env::PathAssignment {
                variables: vec![name],
                value: (*value).into(),
                inheritance: *inheritance,
            })
            .collect()
    }

    fn hash(value: &str) -> u64 {
        crate::hostenv::fnv1a(value.as_bytes())
    }

    /// The one entry line the encoder writes for these fields.
    fn expected_entry(name: &str, policy: char, value: &str, applied: Option<u64>) -> String {
        match applied {
            Some(hash) => {
                format!("{name}={policy}{}:{value}@{hash:016x};\n", value.len())
            }
            None => format!("{name}={policy}{}:{value}-;\n", value.len()),
        }
    }

    #[test]
    fn the_generated_input_lists_changed_and_mapped_variables() {
        let base = map(&[
            ("UNCHANGED", "same"),
            ("MAPPED", "caller"),
            ("CHANGED", "old"),
        ]);
        let now = map(&[
            ("UNCHANGED", "same"),
            ("MAPPED", "caller"),
            ("CHANGED", "new"),
            ("HOOKOUT", "/private/font.conf"),
            (HOST_ENV_VAR, "rec"),
            (ROOTS_VAR, "/view"),
        ]);
        let maps = assignments(&[("MAPPED", "/view/plugins", Inheritance::Preserve)]);
        let bytes = encode_view_env(
            &base,
            &now,
            &maps,
            Some(OsStr::new("/view")),
            Some(OsStr::new("rec")),
        )
        .unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("ONELF-RUNTIME-ENV 1 3\n"));
        assert!(text.contains(&expected_entry(
            "MAPPED",
            '.',
            "/view/plugins",
            Some(hash("caller"))
        )));
        assert!(text.contains(&expected_entry("CHANGED", '!', "new", Some(hash("new")))));
        assert!(text.contains(&expected_entry(
            "HOOKOUT",
            '!',
            "/private/font.conf",
            Some(hash("/private/font.conf"))
        )));
        assert!(!text.contains("UNCHANGED"));
        assert!(!text.contains("ONELF_HOST_ENV="));
        assert!(text.ends_with("R5:/view;\nH3:rec;\n"));
    }

    #[test]
    fn an_unchanged_launch_still_records_its_mappings() {
        let env_state = map(&[("MAPPED", "caller"), ("OTHER", "same")]);
        let maps = assignments(&[("MAPPED", "/view/plugins", Inheritance::Replace)]);
        let bytes = encode_view_env(&env_state, &env_state, &maps, None, None).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("ONELF-RUNTIME-ENV 1 1\n"));
        assert!(text.contains(&expected_entry(
            "MAPPED",
            '!',
            "/view/plugins",
            Some(hash("caller"))
        )));
        assert!(!text.contains("OTHER"));

        let plain = map(&[("OTHER", "same")]);
        let bytes = encode_view_env(&plain, &plain, &[], None, None).unwrap();
        assert!(bytes.starts_with(VIEW_ENV_HEADER));
        assert!(bytes.ends_with(b"0\n"));
    }

    #[test]
    fn a_value_beyond_the_per_entry_bounds_is_skipped() {
        let base = map(&[]);
        let now = map(&[
            ("HUGE", &"x".repeat(40_000)),
            (&"L".repeat(200), "value"),
            ("KEPT", "fine"),
        ]);
        let bytes = encode_view_env(&base, &now, &[], None, None).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("ONELF-RUNTIME-ENV 1 1\n"));
        assert!(text.contains(&expected_entry("KEPT", '!', "fine", Some(hash("fine")))));
        assert!(!text.contains("HUGE"));
    }

    #[test]
    fn an_input_beyond_the_total_cap_is_refused_not_truncated() {
        let base = BTreeMap::new();
        let mut now = BTreeMap::new();
        for i in 0..4000u32 {
            now.insert(
                OsString::from(format!("V{i:04}")),
                OsString::from(format!("{:0300}", "")),
            );
        }
        let got = encode_view_env(&base, &now, &[], None, None);
        assert!(matches!(got, Err(super::ViewEnvError::TooLarge)));
    }

    #[test]
    fn only_a_private_tree_publishes_the_input() {
        let (sender, _receiver) = crate::links::channel().unwrap();
        assert!(publishes_view_env(&super::Links::Tree, true, false));
        assert!(publishes_view_env(&super::Links::Tree, false, true));
        assert!(!publishes_view_env(&super::Links::Tree, false, false));
        assert!(!publishes_view_env(
            &super::Links::Server(&sender),
            true,
            true
        ));
        assert!(!publishes_view_env(&super::Links::Served, true, true));
    }

    #[test]
    fn the_snapshot_is_the_process_environment() {
        let snapshot = snapshot_env();
        assert!(
            snapshot
                .contains_key(OsString::from("PATH").as_os_str())
                .then_some(())
                .is_some()
        );
    }
}

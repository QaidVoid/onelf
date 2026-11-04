//! The last step of every execution mode: decide what the host supplies,
//! set up the environment, and replace this process with the entrypoint.
//!
//! FUSE, tmpfs and cache modes differ in how the package tree comes to
//! exist on disk and in nothing after that, so they share this one path.
//! The memfd mode has no tree and no libraries and does not come here.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use onelf_format::HostLibsPolicy;
use onelf_format::Manifest;
use onelf_format::drivers;
use onelf_format::resolve::{self, Gl, Request, Resolution};

use crate::loader::PackageData;

/// Everything a launch needs beyond the package itself.
pub struct Launch<'a> {
    /// The loaded package the entrypoint runs from.
    pub pkg: &'a PackageData,
    /// The package root as the entrypoint will see it: the mount, the
    /// tmpfs, or the extracted directory.
    pub pkg_root: &'a Path,
    /// Reported to the app as `ONELF_ACTIVE_MODE`.
    pub mode: &'static str,
    /// The manifest entrypoint this launch runs.
    pub ep_idx: usize,
    /// The `argv[0]` the entrypoint sees.
    pub argv0: &'a str,
    /// The package path this process was started from, as
    /// `/proc/self/exe` reports it.
    pub exec_path: &'a str,
    /// The arguments the entrypoint is exec'd with.
    pub args: &'a [OsString],
    /// The package's interpreter record, when it packed one.
    pub interp_data: Option<&'a [u8]>,
    /// The package's custom `[env]` record, when the recipe set one.
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
    /// A resolver decision the caller made before the launch's tree was
    /// materialized, which the mode needed to know the effective links
    /// before it could build or join a view. `None` resolves here.
    pub resolved: Option<Resolved>,
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
pub(crate) fn exec_served(launch: &Launch) {
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
    let lib_dirs = manifest.lib_dirs();

    // The sets the package left its dependencies to, obtained before any
    // mode made a mount namespace of its own.
    let set_roots = crate::sets::roots();
    let set_lib_dirs: Vec<std::path::PathBuf> = set_roots
        .iter()
        .flat_map(|r| crate::sets::lib_dirs(r))
        .collect();

    // What the selected providers can run beside: their objects against
    // the application's bundled libc and loader pair, checked before the
    // resolver edits a tree, the environment is touched, or the app
    // starts. Wrong-architecture objects, needs beyond the bundled pair
    // and the selection, and unrelated libc families all end it here.
    let bundled_interp_rel = launch
        .interp_data
        .and_then(crate::interp::parse_bundled_interp_rel);
    if let Err(why) = crate::providers::validate_selected(
        launch.pkg_root,
        &lib_dirs,
        &target_path,
        bundled_interp_rel,
        &ep_name,
    ) {
        eprintln!("onelf-rt: {why}");
        std::process::exit(1);
    }

    // The resolver judges the package, not the entrypoint: a script
    // execs the bundled binaries too. A mode that had to know the
    // decision before it could materialize its tree hands it in.
    let resolved_owned;
    let resolved = match &launch.resolved {
        Some(resolved) => resolved,
        None => {
            resolved_owned = if lib_dirs.is_empty() {
                Resolved::never()
            } else {
                resolve_launch(
                    launch.pkg,
                    launch.ep_idx,
                    launch.pkg_root,
                    launch.private_ns,
                    launch.tree_writable,
                )
            };
            &resolved_owned
        }
    };
    let resolution = &resolved.resolution;
    for name in &resolution.incomparable {
        eprintln!("onelf-rt: resolver: cannot order {name}; keeping the bundled copy");
    }
    if let Some(farm) = &resolution.farm {
        shadow_bundled_copies(farm, launch, &lib_dirs);
    }
    let runpath = crate::links::origin_runpath(&target_path);
    let wanted = if lib_dirs.is_empty() {
        None
    } else {
        wanted_links(
            launch,
            &lib_dirs,
            &runpath,
            resolution.farm.as_deref(),
            &set_roots,
        )
    };
    if let Some((dir, links)) = &wanted
        && !place_links(launch, dir, links, &lib_dirs)
    {
        return;
    }

    let env_before = snapshot_env();
    let lib_path = apply_env(launch, resolved, &set_lib_dirs, &set_roots);
    publish_view_env(
        launch,
        &env_before,
        &lib_dirs,
        &set_lib_dirs,
        &set_roots,
        wanted.as_ref(),
    );
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

/// The package directory the launch's library links go in, and the links
/// it takes from the farm and the sets, or `None` when the package has no
/// directory for them.
fn wanted_links(
    launch: &Launch,
    lib_dirs: &[&str],
    runpath: &[PathBuf],
    farm: Option<&Path>,
    set_roots: &[PathBuf],
) -> Option<(String, crate::links::Links)> {
    let manifest = &launch.pkg.manifest;
    let dir = crate::links::dir(manifest, launch.pkg_root, runpath, lib_dirs)?;
    let links = crate::links::wanted(manifest, &dir, farm, set_roots);
    Some((dir, links))
}

/// Show `links` in the package's `dir`. False when the mount another
/// launch serves shows a different set, which this launch cannot change.
/// A server this launch started learns them with the generated
/// environment input, in [`publish_view_env`].
fn place_links(launch: &Launch, dir: &str, links: &crate::links::Links, lib_dirs: &[&str]) -> bool {
    let manifest = &launch.pkg.manifest;
    match launch.links {
        Links::Tree => {
            crate::links::place(manifest, launch.pkg_root, dir, links);
            true
        }
        Links::Server(_) => true,
        Links::Served => crate::links::served(manifest, launch.pkg_root, dir, links, lib_dirs),
    }
}

/// Apply the launch's environment: the generated mappings and the mode's
/// own variables, the recipe's custom `[env]`, and the host-restoration
/// record. Returns the library path the mappings computed.
fn apply_env(
    launch: &Launch,
    resolved: &Resolved,
    set_lib_dirs: &[PathBuf],
    set_roots: &[PathBuf],
) -> String {
    let manifest = &launch.pkg.manifest;
    let ep = &manifest.entrypoints[launch.ep_idx];
    let ep_name = manifest.get_string(ep.name);
    let target_path = launch
        .pkg_root
        .join(manifest.entry_path(ep.target_entry as usize));
    let pkg_root_s = launch.pkg_root.to_str().unwrap_or("");
    let lib_path = crate::env::setup_env(
        pkg_root_s,
        launch.argv0,
        launch.exec_path,
        ep_name,
        launch.mode,
        &manifest.lib_dirs().join(":"),
        target_path.to_str().unwrap_or(""),
        resolved.resolution.farm.as_deref(),
        resolved.platform_root.as_deref(),
        set_lib_dirs,
        set_roots,
    );
    if let Some(data) = launch.env_data {
        crate::env::apply_custom_env(data, pkg_root_s);
    }
    crate::hostenv::record_host_env(launch.pkg_root, set_roots);
    lib_path
}

/// A launch's environment as evaluated for its per-view input, without
/// being left applied: see [`generated_env`].
pub struct GeneratedEnv {
    before: BTreeMap<OsString, OsString>,
    applied: BTreeMap<OsString, OsString>,
    assignments: Vec<crate::env::PathAssignment<'static>>,
    set_roots: Vec<PathBuf>,
}

impl GeneratedEnv {
    /// The per-view input as evaluated.
    pub fn encode(&self) -> Result<Vec<u8>, crate::env::ViewEnvError> {
        crate::env::encode_view_env(&self.before, &self.applied, &self.assignments)
    }

    /// The per-view input of a tree that shows the evaluated tree's
    /// content under the root `to` instead of `from`: every value the
    /// launch set has `from` replaced by `to`, and the root and
    /// host-restoration records are made again over those values, so
    /// every length and hash the input carries is the rewritten one's.
    pub fn encode_rooted(
        &self,
        from: &Path,
        to: &Path,
    ) -> Result<Vec<u8>, crate::env::ViewEnvError> {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let (from_b, to_b) = (from.as_os_str().as_bytes(), to.as_os_str().as_bytes());
        let rewrite = |value: &OsString| -> OsString {
            let bytes = value.as_bytes();
            if from_b.is_empty() {
                return value.clone();
            }
            let mut out = Vec::with_capacity(bytes.len());
            let mut rest = bytes;
            while let Some(at) = rest.windows(from_b.len()).position(|w| w == from_b) {
                out.extend_from_slice(&rest[..at]);
                out.extend_from_slice(to_b);
                rest = &rest[at + from_b.len()..];
            }
            out.extend_from_slice(rest);
            OsString::from_vec(out)
        };
        let mut applied: BTreeMap<OsString, OsString> = self
            .applied
            .iter()
            .map(|(name, value)| {
                let value = if self.before.get(name) == Some(value) {
                    value.clone()
                } else {
                    rewrite(value)
                };
                (name.clone(), value)
            })
            .collect();
        crate::hostenv::host_records(&mut applied, to, &self.set_roots);
        let assignments: Vec<crate::env::PathAssignment<'static>> = self
            .assignments
            .iter()
            .map(|a| crate::env::PathAssignment {
                variables: a.variables.clone(),
                value: rewrite(&a.value),
                inheritance: a.inheritance,
            })
            .collect();
        crate::env::encode_view_env(&self.before, &applied, &assignments)
    }
}

/// Evaluate the per-view environment input this launch would publish,
/// against its `pkg_root` with the resolver decision it carries (none
/// takes nothing from the host). This process's environment is left
/// exactly as it was found.
///
/// A mode that has to know the input before its tree exists, because the
/// input is part of the tree's identity or decides whether a shared mount
/// may be joined, evaluates it here.
pub fn generated_env(launch: &Launch) -> GeneratedEnv {
    let lib_dirs = launch.pkg.manifest.lib_dirs();
    let set_roots = crate::sets::roots();
    let set_lib_dirs: Vec<PathBuf> = set_roots
        .iter()
        .flat_map(|r| crate::sets::lib_dirs(r))
        .collect();
    let never;
    let resolved = match &launch.resolved {
        Some(resolved) => resolved,
        None => {
            never = Resolved::never();
            &never
        }
    };
    let before = snapshot_env();
    apply_env(launch, resolved, &set_lib_dirs, &set_roots);
    let assignments =
        crate::env::view_env_assignments(launch.pkg_root, &lib_dirs, &set_lib_dirs, &set_roots);
    let applied = snapshot_env();
    for (name, value) in &before {
        if applied.get(name) != Some(value) {
            // SAFETY: the runtime is single-threaded while it launches.
            unsafe { std::env::set_var(name, value) };
        }
    }
    for name in applied.keys() {
        if !before.contains_key(name) {
            // SAFETY: as above.
            unsafe { std::env::remove_var(name) };
        }
    }
    GeneratedEnv {
        before,
        applied,
        assignments,
        set_roots,
    }
}

/// The environment this launch found, before it changed any of it.
fn snapshot_env() -> BTreeMap<OsString, OsString> {
    std::env::vars_os().collect()
}

/// Whether this launch publishes the per-view environment input itself:
/// into a tree that belongs to this launch alone, or through the server
/// it started. A shared extracted tree or a mount another launch serves
/// is never written to; a cache view carries the input from its
/// publication. Views that carry no input run on the packaged
/// `.onelf/env` alone.
fn publishes_view_env(links: &Links<'_>, tree_writable: bool, private_ns: bool) -> bool {
    match links {
        Links::Tree => tree_writable || private_ns,
        Links::Server(_) => true,
        Links::Served => false,
    }
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
    wanted: Option<&(String, crate::links::Links)>,
) {
    if !publishes_view_env(&launch.links, launch.tree_writable, launch.private_ns) {
        return;
    }
    let assignments =
        crate::env::view_env_assignments(launch.pkg_root, lib_dirs, set_lib_dirs, set_roots);
    let now = snapshot_env();
    let bytes = match crate::env::encode_view_env(base, &now, &assignments) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("onelf-rt: the generated environment input is {e} and is not written");
            Vec::new()
        }
    };
    if let Links::Server(sender) = launch.links {
        let no_links = crate::links::Links::new();
        let (dir, links) = wanted.map_or(("", &no_links), |(d, l)| (d.as_str(), l));
        if !links.is_empty() || !bytes.is_empty() {
            sender.send(dir, links, &bytes);
        }
        return;
    }
    if !bytes.is_empty()
        && let Err(e) = std::fs::write(launch.pkg_root.join(crate::env::VIEW_ENV_FILE), &bytes)
    {
        eprintln!("onelf-rt: cannot write {}: {e}", crate::env::VIEW_ENV_FILE);
    }
}

/// `LD_LIBRARY_PATH` for an ELF without a `$ORIGIN` run path that the
/// kernel starts: the package's library directories, which hold the
/// library links as well.
///
/// Such a binary is one the packer gave no run path: a self-extracting
/// one, which needs the kernel to make `/proc/self/exe` itself, one
/// bundled where neither a slot nor patchelf was there to write one, or
/// one packed without `bundle-libs` that runs under the host's loader. A
/// bootstrapped binary maps its loader itself, so no loader of ours could
/// be handed `--library-path` for it, and a host loader may predate the
/// `--argv0` the command line would need. This is the one place the
/// variable is still set, and what the app spawns inherits it.
///
/// The caller's inherited value is not appended: the application resolves
/// only from its package's own directories, and the caller's value
/// reaches the host programs the app spawns through the restoration
/// record, which lists this variable as set at exec.
fn fallback_library_path(pkg_root: &Path, lib_dirs: &[&str]) -> String {
    lib_dirs
        .iter()
        .map(|d| pkg_root.join(d).to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(":")
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
pub struct Resolved {
    /// The resolver's decision: the link farm, the host loader if it won,
    /// and the sonames that could not be ordered.
    pub resolution: Resolution,
    /// The launch's effective policy over host libraries, after
    /// `ONELF_NO_RESOLVER` had its say.
    pub policy: HostLibsPolicy,
    /// The fetched GL build's extracted root.
    pub platform_root: Option<std::path::PathBuf>,
    /// Held through exec so the extraction is not collected while this
    /// instance runs. The fd is inheritable.
    _platform_lock: Option<std::fs::File>,
}

/// Run the resolver for this package on this host, against the tree at
/// `pkg_root`. `ONELF_NO_RESOLVER` turns it off for one launch, which is
/// the way to tell whether a failure is the resolver's doing.
///
/// A launch that can put no host file in place of a bundled one, neither
/// by mount nor by editing its tree, never takes the host's glibc: the
/// bundled executables would load their own libc under the host loader.
///
/// A host with no GL stack gets the build the package pins, if it pins
/// one and the build can be had; the resolver then indexes that build
/// ahead of the host. Anything that stops that is a warning, and the
/// launch goes on without a GL stack.
pub fn resolve_launch(
    pkg: &PackageData,
    ep_idx: usize,
    pkg_root: &Path,
    private_ns: bool,
    tree_writable: bool,
) -> Resolved {
    let lib_dirs = pkg.manifest.lib_dirs();
    let disabled = std::env::var_os("ONELF_NO_RESOLVER").is_some_and(|v| !v.is_empty() && v != "0");
    let policy = if disabled {
        HostLibsPolicy::Never
    } else {
        HostLibsPolicy::from_flags(pkg.footer.flags)
    };
    if policy == HostLibsPolicy::Never {
        return Resolved::never();
    }
    let Some(store) = crate::paths::resolve_store(&pkg.manifest.header.package_id) else {
        return Resolved::never();
    };
    let ld_cache = drivers::cache_file();

    let mut platform = None;
    // A set holds the toolkit, and the toolkit is what usually names a
    // GL entry point, so its directories count towards the decision.
    let set_roots = crate::sets::roots();
    let set_lib_dirs: Vec<std::path::PathBuf> = set_roots
        .iter()
        .flat_map(|root| crate::sets::lib_dirs(root))
        .collect();
    let ep = &pkg.manifest.entrypoints[ep_idx];
    let executable = pkg_root.join(pkg.manifest.entry_path(ep.target_entry as usize));
    let gl = resolve::gl_situation(
        pkg_root,
        &lib_dirs,
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
        lib_dirs: &lib_dirs,
        policy,
        store: &store,
        ld_cache: &ld_cache,
        icd_dirs: resolve::ICD_DIRS,
        extra_root: platform_root.as_deref(),
        set_roots: &set_roots,
        host_libc: private_ns || tree_writable,
    });
    Resolved {
        resolution,
        policy,
        platform_root,
        _platform_lock: lock,
    }
}

/// A decision that takes nothing from the host: the resolver is off.
impl Resolved {
    fn never() -> Self {
        Self {
            resolution: Resolution::default(),
            policy: HostLibsPolicy::Never,
            platform_root: None,
            _platform_lock: None,
        }
    }
}

/// The library links this launch takes from outside the package, and the
/// directory of the package they go in, as the resolver's farm and the
/// dependency sets decided.
///
/// Modes that materialize their tree or join a mount before the launch
/// runs need the links first: the links are what a view's identity
/// covers and what a shared mount must already serve.
pub fn effective_links(
    manifest: &Manifest,
    ep_idx: usize,
    pkg_root: &Path,
    farm: Option<&Path>,
) -> (Option<String>, crate::links::Links) {
    let lib_dirs = manifest.lib_dirs();
    let target =
        pkg_root.join(manifest.entry_path(manifest.entrypoints[ep_idx].target_entry as usize));
    let runpath = crate::links::origin_runpath(&target);
    let dir = crate::links::dir(manifest, pkg_root, &runpath, &lib_dirs);
    let set_roots = crate::sets::roots();
    let links = match &dir {
        Some(dir) => crate::links::wanted(manifest, dir, farm, &set_roots),
        None => crate::links::Links::new(),
    };
    (dir, links)
}

#[cfg(test)]
mod view_env_tests {
    use super::{Links, publishes_view_env, snapshot_env};
    use std::ffi::OsString;

    #[test]
    fn only_a_private_tree_or_an_own_server_publishes_the_input() {
        let (sender, _receiver) = crate::links::channel().unwrap();
        assert!(publishes_view_env(&Links::Tree, true, false));
        assert!(publishes_view_env(&Links::Tree, false, true));
        assert!(!publishes_view_env(&Links::Tree, false, false));
        assert!(publishes_view_env(&Links::Server(&sender), false, false));
        assert!(!publishes_view_env(&Links::Served, true, true));
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

//! The packed application's runtime binary. It loads the package it was
//! invoked from, decides an execution mode, and replaces itself with the
//! entrypoint through the launch support in this crate's library.
//!
//! The binary owns what is genuinely binary-only: the embedded capability
//! note describing this build, the packed binary's flag handlers
//! (inspection, diagnostics, desktop metadata and integration), the
//! in-memory memfd execution of a self-contained payload, the cache-mode
//! orchestration below, and the updater behind the `update` feature. The
//! updater registers its HTTPS client with the library's acquisition, so
//! the slim build, which leaves the feature off, still links and runs
//! with `file://` acquisition alone.
mod capability;
mod doctor;
mod inspect;
mod integrate;
mod memfd;
mod metadata;
#[cfg(feature = "update")]
mod update;

use std::ffi::OsString;
use std::io::ErrorKind;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;

use onelf_format::EntryKind;
use onelf_format::HostLibsPolicy;
use onelf_rt::cache;
use onelf_rt::env;
use onelf_rt::ephemeral;
use onelf_rt::fuse;
use onelf_rt::guardian;
use onelf_rt::hostenv;
use onelf_rt::launch;
use onelf_rt::links;
use onelf_rt::loader;
use onelf_rt::multicall;
use onelf_rt::paths;
use onelf_rt::pkgmeta;
use onelf_rt::portable;
use onelf_rt::rundir;
use onelf_rt::sets;

fn main() {
    onelf_rt::anchor::reserve();

    // The updater's client serves acquisition's https:// sources. The
    // slim build leaves the update feature, and with it this client,
    // out; acquisition then refuses https:// and serves file:// alone.
    #[cfg(feature = "update")]
    onelf_rt::platform::supply_https_fetch(update::download);

    hostenv::capture_starting_env();

    // Bytes, not text: a file manager hands a GUI app whatever name the
    // file has, and `args()` would abort the launch on one that is not UTF-8.
    let args: Vec<OsString> = std::env::args_os().collect();
    let argv0 = args.first().and_then(|s| s.to_str()).unwrap_or("onelf");

    let exec_path = std::fs::read_link("/proc/self/exe")
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_default();

    let exe_path = Path::new(&exec_path);
    let exe_dir = exe_path.parent().unwrap_or(Path::new("."));
    let exe_name = exe_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("onelf");

    if inspect::handle_help(&args) {
        return;
    }

    // Handle --onelf-portable-* flags (create dirs and exit)
    if portable::handle_portable_flags(&args, exe_dir, exe_name) {
        return;
    }

    let mut pkg = match loader::load() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("onelf-rt: failed to load package: {e}");
            std::process::exit(1);
        }
    };

    let ep_idx = multicall::resolve_entrypoint(&pkg.manifest, argv0);

    if ep_idx >= pkg.manifest.entrypoints.len() {
        eprintln!("onelf-rt: no valid entrypoint found");
        std::process::exit(1);
    }

    let ep_name = pkg
        .manifest
        .get_string(pkg.manifest.entrypoints[ep_idx].name)
        .to_string();

    // The launch gate for versioned payload metadata: a record that is
    // present but unreadable, truncated, unsupported, tampered,
    // self-inconsistent, or not backed by the set table and the
    // entrypoints ends the launch here, before the memfd read, set
    // acquisition, hooks or any environment change. A package with neither
    // record launches with the legacy defaults. The invoked entrypoint's
    // pins are acquired later, once no mode can still run without them.
    let selected = match pkgmeta::validate(&pkg, &ep_name) {
        Ok(meta) => {
            if let Some(rules) = meta.rules {
                env::carry_rules(rules);
            }
            meta.pins
        }
        Err(e) => {
            eprintln!("onelf-rt: {e}");
            std::process::exit(1);
        }
    };

    // Ownership is judged before any mode enters a user namespace, where
    // root's files and another user's look the same.
    onelf_rt::hostpaths::judge_host_trust(&pkg);

    // Handle --onelf-icon / --onelf-desktop before dispatching
    if metadata::handle_metadata_flags(&args, &mut pkg, &ep_name) {
        return;
    }

    // Handle --onelf-integrate / --onelf-unintegrate
    if integrate::handle_integrate_flags(&args, &mut pkg, &ep_name, &exec_path) {
        return;
    }

    if let Some(code) = inspect::handle_flags(&args, &mut pkg, ep_idx) {
        std::process::exit(code);
    }

    // Handle --onelf-update / --onelf-check-update (only when built with the
    // "update" feature; the slim runtime omits these to save ~1.3 MB).
    #[cfg(feature = "update")]
    if let Some(flag) = update::parse_flag(&args) {
        let Some(url_bytes) = loader::read_package_file(&mut pkg, ".onelf/update-url") else {
            eprintln!("onelf-rt: no update URL configured (repack with --update-url)");
            std::process::exit(1);
        };
        let url = match std::str::from_utf8(&url_bytes) {
            Ok(s) => s.trim().to_string(),
            Err(_) => {
                eprintln!("onelf-rt: update URL is not valid UTF-8");
                std::process::exit(2);
            }
        };
        // The package's own metadata picks the policy: a signing keyring
        // verifies every update, the unsigned opt-in skips verification
        // loudly, and a URL with neither leaves self-update disabled. A
        // package carrying both the key and the marker is not merely
        // unconfigured but contradictory, so it exits 2 rather than the
        // benign disabled 1.
        let unsigned = loader::read_package_file(&mut pkg, ".onelf/update-unsigned").is_some();
        let key_bytes = loader::read_package_file(&mut pkg, ".onelf/update-key");
        let tampered = unsigned && key_bytes.is_some();
        let key = match update::policy(key_bytes, unsigned) {
            Ok(key) => key,
            Err(why) => {
                eprintln!("onelf-rt: {why}");
                std::process::exit(if tampered { 2 } else { 1 });
            }
        };
        let Some(self_path) = update::self_path() else {
            eprintln!("onelf-rt: cannot resolve /proc/self/exe");
            std::process::exit(2);
        };
        // A revision entry that does not read back as a number marks a
        // broken package. Treating it as revisionless would let any
        // release through the downgrade gate.
        let current_revision = match update::package_revision(&mut pkg) {
            Ok(rev) => rev,
            Err(why) => {
                eprintln!("onelf-rt: this package's {why}; refusing to update");
                std::process::exit(2);
            }
        };
        std::process::exit(update::run(
            flag,
            &self_path,
            &url,
            key.as_deref(),
            current_revision,
        ));
    }

    let ep_target_entry = pkg.manifest.entrypoints[ep_idx].target_entry as usize;
    // A memfd exec has no package tree, so the directories anchored paths
    // stand for exist nowhere: a package with anchors runs from a tree.
    let anchored = onelf_rt::anchor::carried(&pkg);
    let ep_memfd = pkg.manifest.entrypoints[ep_idx].is_memfd_eligible() && !anchored;

    let target_blocks = pkg.manifest.entries[ep_target_entry].blocks.clone();
    let target_hash = pkg.manifest.entries[ep_target_entry].content_hash;

    let ep_args_str = pkg
        .manifest
        .get_string(pkg.manifest.entrypoints[ep_idx].args)
        .to_string();
    let extra_args: Vec<OsString> = if ep_args_str.is_empty() {
        Vec::new()
    } else {
        ep_args_str.split('\x1f').map(OsString::from).collect()
    };

    // Build final args: extra_args + remaining argv (skip argv[0])
    let mut final_args = extra_args;
    if args.len() > 1 {
        final_args.extend_from_slice(&args[1..]);
    }

    // `ONELF_MODE` forces a mode; the mode actually chosen is reported
    // separately as `ONELF_ACTIVE_MODE`, so a packed app that launches
    // another one does not hand it a directive. Default order:
    // memfd (if eligible) -> fuse -> tmpfs -> rundir -> cache.
    let forced_mode = std::env::var("ONELF_MODE").ok();
    let force = forced_mode.as_deref();
    let cache_preferred = pkg
        .footer
        .flags
        .contains(onelf_format::Flags::CACHE_PREFERRED);

    // Memfd mode: single static binary, no libs needed
    if force == Some("memfd") || (force.is_none() && ep_memfd && !cache_preferred) {
        // Forcing the mode cannot make a linked entrypoint work here. Its
        // libraries live inside the package and a memfd exec never puts them
        // on disk, so the loader fails on the first one it cannot find. This
        // is the last point at which anything can be said about it: `exec` of
        // a valid ELF succeeds, and the loader's failure happens afterwards,
        // in a process that is no longer ours.
        if force == Some("memfd") && anchored {
            eprintln!(
                "onelf-rt: the package anchors directories its libraries open, which \
                 memfd mode cannot provide: it keeps no package tree for them to name. \
                 Unset ONELF_MODE to let the runtime pick a mode that can."
            );
            std::process::exit(1);
        }
        if force == Some("memfd") && !ep_memfd {
            eprintln!(
                "onelf-rt: entrypoint '{ep_name}' has shared library \
                 dependencies, which memfd mode cannot satisfy: the libraries \
                 exist only inside the package. Unset ONELF_MODE to let the \
                 runtime pick a mode that can."
            );
            std::process::exit(1);
        }

        // Verify the memfd payload against the entrypoint's content hash
        // so an in-memory exec never runs unverified bytes.
        let verified =
            loader::read_payload_blocks(&pkg.file, &pkg.footer, &target_blocks, &pkg.codec)
                .ok()
                .filter(|d| blake3::hash(d).as_bytes() == &target_hash);
        if let Some(data) = verified {
            let lib_paths_str = pkg.manifest.lib_dirs().join(":");
            // memfd mode: target is the memfd data itself (an ELF we
            // just read). Pass a non-empty marker so setup_env treats
            // it as an ELF.
            let _ = env::setup_env(
                "",
                argv0,
                &exec_path,
                &ep_name,
                "memfd",
                &lib_paths_str,
                "/proc/self/fd/0",
                None,
                None,
                &[],
                &[],
                &[],
            );
            portable::setup_portable(exe_dir, exe_name);
            if let Some(env_data) = loader::read_package_file(&mut pkg, ".onelf/env") {
                env::apply_custom_env(&env_data, "");
            }

            if let Err(e) = memfd::execute_memfd(&data, argv0, &final_args)
                && force == Some("memfd")
            {
                eprintln!("onelf-rt: memfd execution failed: {e}");
                std::process::exit(1);
            }
        } else if force == Some("memfd") {
            eprintln!("onelf-rt: failed to read payload for memfd");
            std::process::exit(1);
        }
    }

    // Read interpreter metadata for cross-libc portability (if packed with interp patching)
    let interp_data = loader::read_package_file(&mut pkg, ".onelf/interp");

    // Read custom environment variables from recipe [env] section
    let env_data = loader::read_package_file(&mut pkg, ".onelf/env");

    // The dependency sets the package left its libraries to. Obtained
    // before any mode is chosen: a set is served from the host mount
    // namespace so every package naming it shares one mount, and a mode
    // that unshares first would make that mount private to this launch.
    // The carried selection record scopes acquisition to the invoked
    // entrypoint's selection, which the launch gate resolved and checked.
    onelf_rt::notify::set_app(match pkg.manifest.name() {
        "" => &ep_name,
        name => name,
    });
    sets::init(&selected, &ep_name);

    // The app runs setuid binaries, so it has to stay in the namespace it
    // was started in. That rules out the two modes that make one of their
    // own, leaving fusermount3 and, failing that, extraction. The file's
    // presence is the whole message; it holds nothing.
    let needs_setuid = loader::read_package_file(&mut pkg, ".onelf/needs-setuid").is_some();

    // The publisher put the cache first: the app records its own path in
    // files that outlive it, so that path has to stay on disk. The other
    // modes still run when the extraction fails.
    if force.is_none() && cache_preferred {
        let err = exec_cache(
            &mut pkg,
            ep_idx,
            argv0,
            &exec_path,
            &final_args,
            interp_data.as_deref(),
            env_data.as_deref(),
            exe_dir,
            exe_name,
        );
        eprintln!("onelf-rt: {err}; trying the other execution modes");
    }

    // FUSE mode: mount package as filesystem (default for non-memfd)
    if matches!(force, None | Some("fuse")) {
        fuse::execute_fuse(
            &mut pkg,
            ep_idx,
            argv0,
            &exec_path,
            &final_args,
            interp_data.as_deref(),
            env_data.as_deref(),
            needs_setuid,
        );
        // Only reaches here if FUSE fell back
        if force == Some("fuse") {
            eprintln!("onelf-rt: FUSE mode unavailable");
            std::process::exit(1);
        }
    }

    // Ephemeral tmpfs mode: private namespace + tmpfs + extract. Invisible
    // to the host, no persistent on-disk artifacts. Preferred over cache
    // mode whenever user namespaces are available.
    if force == Some("tmpfs") && needs_setuid {
        eprintln!(
            "onelf-rt: tmpfs mode needs a user namespace, and this package \
             runs setuid binaries, so it does not use one. Unset ONELF_MODE \
             to let the runtime pick a mode that can."
        );
        std::process::exit(1);
    }
    if matches!(force, None | Some("tmpfs")) && !needs_setuid {
        ephemeral::execute_tmpfs(
            &mut pkg,
            ep_idx,
            argv0,
            &exec_path,
            &final_args,
            interp_data.as_deref(),
            env_data.as_deref(),
        );
        if force == Some("tmpfs") {
            eprintln!("onelf-rt: tmpfs mode unavailable");
            std::process::exit(1);
        }
    }

    // Runtime directory mode: extraction into the private per-user
    // directory, cleaned up when the last user is gone. Needs neither a
    // namespace nor a helper, so it is the last mode that leaves nothing
    // behind.
    if matches!(force, None | Some("rundir")) {
        rundir::execute_rundir(
            &mut pkg,
            ep_idx,
            argv0,
            &exec_path,
            &final_args,
            interp_data.as_deref(),
            env_data.as_deref(),
        );
        if force == Some("rundir") {
            eprintln!("onelf-rt: rundir mode unavailable");
            std::process::exit(1);
        }
    }

    // The persistent cache leaves an extraction on disk, so it runs only
    // when asked for: by the publisher at pack time, by the user for this
    // launch, or by forcing the mode.
    let cache_requested = pkg
        .footer
        .flags
        .contains(onelf_format::Flags::CACHE_REQUESTED)
        || std::env::var_os("ONELF_CACHE").is_some_and(|v| !v.is_empty() && v != "0");
    if force != Some("cache") && !cache_requested {
        eprintln!(
            "onelf-rt: no execution mode is available on this host; set ONELF_CACHE=1 \
             to allow a persistent extraction under the cache directory"
        );
        std::process::exit(1);
    }

    let err = exec_cache(
        &mut pkg,
        ep_idx,
        argv0,
        &exec_path,
        &final_args,
        interp_data.as_deref(),
        env_data.as_deref(),
        exe_dir,
        exe_name,
    );
    eprintln!("onelf-rt: {err}");
    std::process::exit(1);
}

/// Run the entrypoint from the persistent cache. Returns only when the
/// package cannot be run from there.
///
/// The shared base extraction is never run from and never edited: the
/// launch publishes an execution view of its own, keyed by its full
/// identity, beside it. The view stages the whole package by hardlinks
/// out of the verified base, installs this launch's library links in
/// staging, and is published once, completely, ready record and all. Two
/// launches whose selections, host decisions, or entrypoints differ get
/// different views, so neither ever adds, removes, or rewrites a link
/// under the other; two launches with one identity share the view, whose
/// published links never change. The launch runs from the view under the
/// lifetime guardian, which holds the base extraction's in-use lock and
/// the view's in-use lease itself, so the claims outlive the foreground
/// process however thoroughly a descendant discards what it inherited.
// Describes a single exec; every argument is distinct and a parameter
// object would be built at each call site and read once.
#[allow(clippy::too_many_arguments)]
fn exec_cache(
    pkg: &mut loader::PackageData,
    ep_idx: usize,
    argv0: &str,
    exec_path: &str,
    args: &[OsString],
    interp_data: Option<&[u8]>,
    env_data: Option<&[u8]>,
    exe_dir: &Path,
    exe_name: &str,
) -> std::io::Error {
    // The lock guard is held by the guardian for the lifetime of this
    // instance, so a concurrent process's GC cannot delete the package
    // while it is still in use.
    let (pkg_dir, pkg_lock) = match cache::ensure_extracted(pkg) {
        Ok(d) => d,
        Err(e) => return std::io::Error::new(e.kind(), format!("extraction failed: {e}")),
    };

    let package_id = cache::hex(&pkg.manifest.header.package_id);

    let target_entry = pkg.manifest.entrypoints[ep_idx].target_entry as usize;
    let target_path = pkg_dir.join(pkg.manifest.entry_path(target_entry));
    if !target_path.exists() {
        return std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "entrypoint target does not exist: {}",
                target_path.display()
            ),
        );
    }

    // The resolver decision is made before the view exists: which host
    // libraries win decides the links, and the links are part of the
    // view's identity. The base shows the same package content the view
    // will, so judging it here is judging the view.
    let lib_dirs_empty = pkg.manifest.lib_dirs().is_empty();
    let resolved = (!lib_dirs_empty).then(|| launch::resolve_launch(pkg, ep_idx, &pkg_dir, false));
    let policy = resolved
        .as_ref()
        .map(|r| r.policy)
        .unwrap_or(HostLibsPolicy::Never);
    let (link_dir, links) = launch::effective_links(
        &pkg.manifest,
        ep_idx,
        &pkg_dir,
        resolved.as_ref().and_then(|r| r.resolution.farm.as_deref()),
    );

    let ep_name = pkg
        .manifest
        .get_string(pkg.manifest.entrypoints[ep_idx].name)
        .to_string();
    // The evaluated environment input belongs to the view: it is part of
    // the identity, and the view carries it, so an application that
    // clears its environment and re-execs still finds the generated
    // mappings. It is evaluated against the base, which shows the same
    // content; the identity normalizes the base's path, and the view
    // carries the input made again for its own root.
    let (generated, resolved) = {
        let mut probe = launch::Launch {
            pkg,
            pkg_root: &pkg_dir,
            mode: "cache",
            ep_idx,
            argv0,
            exec_path,
            args,
            interp_data,
            env_data,
            private_ns: false,
            tree_writable: false,
            links: launch::Links::Served,
            resolved,
        };
        (launch::generated_env(&probe), probe.resolved.take())
    };
    let runtime_env = match generated.encode() {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("onelf-rt: the generated environment input is {e} and is not written");
            Vec::new()
        }
    };

    let pins = sets::pins();
    let metadata = metadata_digest(
        loader::read_package_file(pkg, pkgmeta::RUNTIME_RULES_FILE),
        loader::read_package_file(pkg, pkgmeta::ENTRYPOINT_SETS_FILE),
    );
    let inputs = links::view::ViewInputs {
        uid: paths::uid(),
        package: &pkg.manifest.header.package_id,
        metadata: metadata.as_ref(),
        pins: &pins,
        policy,
        host_libc: false,
        loader: resolved.as_ref().and_then(|r| {
            r.resolution
                .host_interp
                .as_ref()
                .map(|l| l.path.as_os_str())
        }),
        entrypoint: Some(&ep_name),
        links: &links,
        runtime_env: &runtime_env,
        root: pkg_dir.as_os_str(),
    };
    let Some(identity) = links::view::ViewIdentity::new(&inputs) else {
        return std::io::Error::new(
            ErrorKind::InvalidInput,
            "the launch's execution-view identity exceeds the record bounds",
        );
    };

    let Some(cache_base) = cache::base_dir() else {
        return std::io::Error::new(
            ErrorKind::NotFound,
            "no safe cache directory (set HOME or XDG_RUNTIME_DIR)",
        );
    };
    let store = cache::view_store(&cache_base);
    if let Err(e) = std::fs::DirBuilder::new().mode(0o700).create(&store)
        && e.kind() != ErrorKind::AlreadyExists
    {
        return std::io::Error::new(e.kind(), format!("the view store: {e}"));
    };
    let Some(view_name) = links::view::dir_name(identity.key()) else {
        return std::io::Error::new(
            ErrorKind::InvalidInput,
            "the launch's execution-view identity names no directory",
        );
    };
    let view_dir = store.join(&view_name);

    // The view's in-use lease, created and locked before the view is
    // resolved or built, inheritable, and taken along by the guardian. It
    // is the claim the cache's collection keys the view's idleness on.
    let Some(lease_path) = links::view::lease_lock_path(&view_dir) else {
        return std::io::Error::new(
            ErrorKind::InvalidInput,
            "the launch's execution-view identity names no lease",
        );
    };
    let lease = match paths::open_lock_inheritable(&lease_path) {
        Ok(f) => f,
        Err(e) => return std::io::Error::new(e.kind(), format!("the view's lease: {e}")),
    };
    use rustix::fs::FlockOperation;
    if rustix::fs::flock(&lease, FlockOperation::LockShared).is_err() {
        return std::io::Error::other("the view's lease cannot be locked");
    }

    // Auto-GC: prune stale cache entries and reclaim the view store's
    // idle views (best-effort). Only now, with this launch's view leased,
    // so the sweep cannot take the view the launch is about to use.
    let gc_max_age = cache::gc_max_age_days();
    if gc_max_age > 0 {
        cache::auto_gc(&cache_base, gc_max_age.saturating_mul(86400), &package_id);
    }

    let built = links::view::ensure_view(&store, &identity, &lease, |staging| {
        stage_view_from_base(staging, &pkg.manifest, &pkg.codec, &pkg_dir)?;
        if let Some(dir) = &link_dir {
            staging.place_links(&pkg.manifest, dir, &links)?;
        }
        if runtime_env.is_empty() {
            return Ok(None);
        }
        let expanded = generated.encode_rooted(&pkg_dir, &view_dir).map_err(|e| {
            std::io::Error::other(format!("the generated environment input is {e}"))
        })?;
        Ok(Some(expanded))
    });
    let view = match built {
        Ok(view) => view,
        Err(e) => {
            return std::io::Error::new(e.kind(), format!("execution view failed: {e}"));
        }
    };
    let view_root = view.dir().to_path_buf();

    portable::setup_portable(exe_dir, exe_name);

    // The guardian owns the launch from here: it holds the base
    // extraction's in-use lock and the view's lease itself, forks the
    // application, reports its exit status the moment it is reaped,
    // adopts the descendants a daemonization leaves behind, and lets the
    // claims go only once a drained `wait` proves its subtree empty. The
    // lock inodes stay on disk; the protocol keys every claim on them.
    struct Claims {
        _pkg: std::fs::File,
        _view: std::fs::File,
    }
    let supervision = match guardian::spawn(
        Claims {
            _pkg: pkg_lock,
            _view: lease,
        },
        || {
            launch::exec(&launch::Launch {
                pkg,
                pkg_root: &view_root,
                mode: "cache",
                ep_idx,
                argv0,
                exec_path,
                args,
                interp_data,
                env_data,
                private_ns: false,
                tree_writable: false,
                links: launch::Links::Served,
                resolved,
            })
        },
        |_claims| {},
    ) {
        Ok(supervision) => supervision,
        Err(e) => {
            eprintln!("onelf-rt: cache: fork failed: {e}");
            std::process::exit(1);
        }
    };

    // Signals sent to this process still reach the application, one hop
    // further along, and this process ends the way the application did.
    guardian::proxy(supervision, "cache")
}

/// Fill a view's staging tree from the verified base extraction. Every
/// package file is hardlinked out of the base, which shares the verified
/// bytes without copying them, every symlink is recreated under the
/// validation the staging surface applies, and the base itself is never
/// touched.
fn stage_view_from_base(
    staging: &links::view::Staging,
    manifest: &onelf_format::Manifest,
    codec: &onelf_format::payload::Codec,
    base: &Path,
) -> std::io::Result<()> {
    for (i, entry) in manifest.entries.iter().enumerate() {
        if entry.kind == EntryKind::Dir {
            let rel = manifest.validated_entry_path(i)?;
            if rel.as_os_str().is_empty() {
                continue;
            }
            std::fs::create_dir_all(staging.dir().join(rel))?;
        }
    }
    for i in onelf_format::payload::file_order(manifest, codec) {
        let rel = manifest.validated_entry_path(i)?;
        let rel = rel
            .to_str()
            .ok_or_else(|| std::io::Error::new(ErrorKind::InvalidData, "entry path"))?;
        staging.hardlink(&base.join(rel), rel)?;
    }
    for (i, entry) in manifest.entries.iter().enumerate() {
        if entry.kind != EntryKind::Symlink {
            continue;
        }
        let rel = manifest.validated_entry_path(i)?;
        let rel = rel
            .to_str()
            .ok_or_else(|| std::io::Error::new(ErrorKind::InvalidData, "entry path"))?;
        staging.symlink(rel, manifest.get_string(entry.symlink_target))?;
    }
    Ok(())
}

/// The content digest of the validated runtime metadata a launch applies,
/// or `None` for a package on the built-in defaults. The digest covers
/// the records' whole bytes, length-prefixed, so any change to validated
/// metadata yields another execution-view identity.
pub(crate) fn metadata_digest(rules: Option<Vec<u8>>, table: Option<Vec<u8>>) -> Option<[u8; 32]> {
    if rules.is_none() && table.is_none() {
        return None;
    }
    let mut hasher = blake3::Hasher::new();
    for part in [rules, table] {
        if let Some(bytes) = part {
            hasher.update(&(bytes.len() as u64).to_le_bytes());
            hasher.update(&bytes);
        } else {
            hasher.update(&0u64.to_le_bytes());
        }
    }
    Some(*hasher.finalize().as_bytes())
}

#[cfg(test)]
mod cache_view_tests {
    use super::{metadata_digest, stage_view_from_base};
    use onelf_format::entry::{Entry, EntryKind};
    use onelf_format::payload::Codec;
    use onelf_format::{Manifest, ManifestHeader};
    use onelf_rt::links;
    use std::cell::Cell;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A sequence number keeping every test's directories apart.
    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "onelf-cache-view-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The in-use lease of `identity`'s view in `store`, held shared as a
    /// launch holds it.
    fn lease(store: &Path, identity: &links::view::ViewIdentity) -> std::fs::File {
        std::fs::create_dir_all(store).unwrap();
        let name = links::view::dir_name(identity.key()).unwrap();
        let path = links::view::lease_lock_path(&store.join(name)).unwrap();
        // Close-on-exec, unlike a launch's: a test worker another thread
        // spawns must not inherit the claim.
        let file = std::fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        rustix::fs::flock(&file, rustix::fs::FlockOperation::LockShared).unwrap();
        file
    }

    /// A manifest whose entries are `(name, kind, parent, symlink target
    /// offset)`, over a string table that holds every name and target.
    fn manifest(parts: &[(&str, EntryKind, u32, Option<&str>)]) -> Manifest {
        let mut table: Vec<u8> = Vec::new();
        let offset = |text: &str, table: &mut Vec<u8>| {
            let at = table.len() as u32;
            table.extend_from_slice(text.as_bytes());
            table.push(0);
            at
        };
        let entries: Vec<Entry> = parts
            .iter()
            .map(|(name, kind, parent, target)| Entry {
                kind: *kind,
                parent: *parent,
                name: offset(name, &mut table),
                mode: match kind {
                    EntryKind::Dir => 0o755,
                    _ => 0o644,
                },
                mtime_secs: 0,
                mtime_nsec: 0,
                content_hash: [0; 32],
                blocks: Vec::new(),
                symlink_target: target.map(|t| offset(t, &mut table)).unwrap_or_default(),
            })
            .collect();
        Manifest {
            header: ManifestHeader {
                version: 3,
                entry_count: entries.len() as u32,
                string_table_size: table.len() as u32,
                entrypoint_count: 0,
                default_entrypoint: 0,
                lib_dir_count: 0,
                name_offset: 0,
                package_id: [1; 32],
                prefix_hash: [0; 32],
                footer_hash: [0; 32],
            },
            entrypoints: Vec::new(),
            entries,
            lib_dir_offsets: Vec::new(),
            string_table: table,
        }
    }

    /// A base extraction carrying one library and one in-package link.
    fn base() -> (Manifest, PathBuf) {
        let manifest = manifest(&[
            ("lib", EntryKind::Dir, u32::MAX, None),
            ("libreal.so", EntryKind::File, 0, None),
            ("liblink.so", EntryKind::Symlink, 0, Some("libreal.so")),
        ]);
        let dir = scratch("base");
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join("lib/libreal.so"), b"verified bytes").unwrap();
        (manifest, dir)
    }

    /// One cache view of the fixture, published through the full
    /// protocol: staging, record, ready marker, one atomic rename.
    fn publish(manifest: &Manifest, base: &Path) -> (PathBuf, PathBuf) {
        let store = scratch("store");
        let identity = links::view::ViewIdentity::new(&links::view::ViewInputs {
            uid: 1000,
            package: &[1; 32],
            metadata: None,
            pins: &[],
            policy: onelf_format::HostLibsPolicy::Never,
            host_libc: false,
            loader: None,
            entrypoint: Some("app"),
            links: &links::Links::new(),
            runtime_env: &[],
            root: base.as_os_str(),
        })
        .unwrap();
        let view =
            links::view::ensure_view(&store, &identity, &lease(&store, &identity), |staging| {
                stage_view_from_base(staging, manifest, &Codec::Zstd(None), base)?;
                Ok(None)
            })
            .unwrap();
        (store, view.dir().to_path_buf())
    }

    /// The staged view carries the base's content as shared inodes and
    /// its own links, and the base itself is untouched.
    #[test]
    fn a_staged_view_shares_the_base_bytes_and_carries_the_links() {
        let (manifest, base) = base();
        let before = std::fs::metadata(base.join("lib/libreal.so"))
            .unwrap()
            .ino();

        let (_store, view) = publish(&manifest, &base);

        let staged = view.join("lib/libreal.so");
        assert_eq!(std::fs::read(&staged).unwrap(), b"verified bytes");
        assert_eq!(
            std::fs::metadata(&staged).unwrap().ino(),
            before,
            "the view shares the base's verified inode"
        );
        assert_eq!(
            std::fs::read_link(view.join("lib/liblink.so")).unwrap(),
            PathBuf::from("libreal.so")
        );
        assert!(links::view::ready(&view), "the view is published ready");
        assert_eq!(
            std::fs::read(base.join("lib/libreal.so")).unwrap(),
            b"verified bytes",
            "the base is untouched"
        );
    }

    /// One identity's view serves every launch that computes it: the
    /// second publication reuses the published view and builds nothing.
    #[test]
    fn a_second_launch_of_one_identity_reuses_the_view() {
        let (manifest, base) = base();
        let store = scratch("store");
        let identity = links::view::ViewIdentity::new(&links::view::ViewInputs {
            uid: 1000,
            package: &[2; 32],
            metadata: None,
            pins: &[],
            policy: onelf_format::HostLibsPolicy::Never,
            host_libc: false,
            loader: None,
            entrypoint: Some("app"),
            links: &links::Links::new(),
            runtime_env: &[],
            root: base.as_os_str(),
        })
        .unwrap();

        links::view::ensure_view(&store, &identity, &lease(&store, &identity), |staging| {
            stage_view_from_base(staging, &manifest, &Codec::Zstd(None), &base)?;
            Ok(None)
        })
        .unwrap();

        let rebuilt = Cell::new(false);
        let again =
            links::view::ensure_view(&store, &identity, &lease(&store, &identity), |_staging| {
                rebuilt.set(true);
                Ok(None)
            })
            .unwrap();
        assert!(
            !rebuilt.get(),
            "a ready view of the same identity is reused, not rebuilt"
        );
        assert!(links::view::ready(again.dir()));
    }

    /// The staged view's library links are placed through the staging
    /// surface and are exactly the links the launch wanted.
    #[test]
    fn the_staged_view_carries_exactly_the_wanted_links() {
        let manifest = manifest(&[
            ("lib", EntryKind::Dir, u32::MAX, None),
            ("libreal.so", EntryKind::File, 0, None),
        ]);
        let base = scratch("links");
        std::fs::create_dir_all(base.join("lib")).unwrap();
        std::fs::write(base.join("lib/libreal.so"), b"x").unwrap();

        let (_store, view) = {
            let store = scratch("store");
            let mut links = links::Links::new();
            links.insert(
                "libhost.so.1".to_string(),
                PathBuf::from("/host/lib/libhost.so.1"),
            );
            let identity = links::view::ViewIdentity::new(&links::view::ViewInputs {
                uid: 1000,
                package: &[3; 32],
                metadata: None,
                pins: &[],
                policy: onelf_format::HostLibsPolicy::Never,
                host_libc: false,
                loader: None,
                entrypoint: Some("app"),
                links: &links,
                runtime_env: &[],
                root: base.as_os_str(),
            })
            .unwrap();
            let view =
                links::view::ensure_view(&store, &identity, &lease(&store, &identity), |staging| {
                    stage_view_from_base(staging, &manifest, &Codec::Zstd(None), &base)?;
                    staging.place_links(&manifest, "lib", &links)?;
                    Ok(None)
                })
                .unwrap();
            (store, view.dir().to_path_buf())
        };

        assert_eq!(
            std::fs::read_link(view.join("lib/libhost.so.1")).unwrap(),
            PathBuf::from("/host/lib/libhost.so.1")
        );
        // What a later launch's verify finds is exactly what was staged.
        assert!(links::served(
            &manifest,
            &view,
            "lib",
            &{
                let mut links = links::Links::new();
                links.insert(
                    "libhost.so.1".to_string(),
                    PathBuf::from("/host/lib/libhost.so.1"),
                );
                links
            },
            &["lib"],
        ));
    }

    /// Validated metadata yields a digest, any change yields another,
    /// and the built-in defaults yield none.
    #[test]
    fn metadata_digests_separate_what_the_launch_applies() {
        assert_eq!(metadata_digest(None, None), None);
        let rules = Some(b"a".to_vec());
        let table = Some(b"b".to_vec());
        let base = metadata_digest(rules.clone(), table.clone()).unwrap();
        assert_eq!(metadata_digest(rules.clone(), table.clone()), Some(base));
        assert_ne!(
            metadata_digest(Some(b"changed".to_vec()), table.clone()),
            Some(base)
        );
        assert_ne!(
            metadata_digest(rules.clone(), Some(b"changed".to_vec())),
            Some(base)
        );
        assert_ne!(metadata_digest(None, table), Some(base));
    }
}

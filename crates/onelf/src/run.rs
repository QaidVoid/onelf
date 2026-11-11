//! Run an AppDir in place for fast dev iteration.
//!
//! If the AppDir has an `onelf.toml`, use it for entrypoint/args/working-dir.
//! Otherwise auto-discover a single binary under `bin/` and run with defaults.
//! The launch runs from a prepared private development view: the tree is
//! staged once, without compression, into a per-view directory under the
//! cache root, and reused untouched while nothing that changes what it
//! shows or restores has changed. The staged view carries the evaluated
//! runtime mappings in the generated file the payload constructor reads
//! back, the library links the launch resolved, and the recipe's
//! absolute-path overlays, so the environment the entrypoint starts with
//! is the one a packed launch starts with.
//!
//! `--bundle` assembles through the same machinery a build uses and
//! prepares its launch through the shared runtime support: the recipe's
//! sysroot, sets, rules, exclusions and entrypoints resolve once into an
//! assembly request, the selected entrypoint's sets and the pinned GL
//! build are acquired by the shared store with the CLI's own HTTPS
//! client, and the launch resolves against what was acquired.

use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use onelf_format::cache_layout as layout;
use onelf_format::resolve::{self, Gl, Request, Resolution};
use onelf_format::{HostLibsPolicy, WorkingDir, drivers};

use crate::assembly;
use crate::pack::HostLibs;
use crate::recipe;

/// The AppDir a run targets, with the recipe found beside it.
struct Loaded {
    /// The recipe file an assembly request resolves from.
    path: PathBuf,
    /// The loaded recipe.
    recipe: recipe::Recipe,
}

/// What a bundle launch assembles before it resolves its entrypoint.
struct Assembled {
    /// The shared assembly request a build would pack from, carrying
    /// the resolved selection table a build lowers into its record.
    request: assembly::AssemblyRequest,
    /// The pins the assembly prepared resources into, which bind runtime
    /// records exactly as a build's would.
    prepared: Vec<crate::bundle::PreparedPin>,
}

/// Assemble the AppDir for a bundle launch the way a build assembles it:
/// the recipe resolves once into the shared assembly request, staging
/// the sysroot contents, the recorded sets, the rule-driven resources
/// and the bundle exclusions the recipe names, and refusing a
/// declaration a build would refuse while the AppDir stands untouched.
///
/// Acquisition never carries its own network stack: the CLI's existing
/// HTTPS client is handed to the shared store before the first
/// acquisition, so pinned artifacts over `https://` come from the same
/// client every publisher-side fetch uses, and the updater feature of
/// the runtime stays out of this build.
fn assemble_for_launch(loaded: Loaded) -> io::Result<Assembled> {
    onelf_rt::platform::supply_https_fetch(https_fetch);
    let request = assembly::AssemblyRequest::plan(&loaded.path, loaded.recipe, None)?;
    let report = request.stage()?;
    Ok(Assembled {
        request,
        prepared: report.prepared,
    })
}

/// Install `records` as the launch's runtime rule record, through the
/// same lowering and codec a build packs them with, so a development
/// launch evaluates exactly the canonical record a packed one reads.
fn carry_rules(records: Vec<onelf_format::runtime_metadata::RuntimeRuleRecord>) -> io::Result<()> {
    let invalid = |e: onelf_format::runtime_metadata::MetadataError| {
        io::Error::new(io::ErrorKind::InvalidData, format!("runtime rules: {e}"))
    };
    let lowered = crate::pack::lower_runtime_metadata(records, None).map_err(invalid)?;
    if let Some(bytes) = lowered.runtime_rules {
        let decoded =
            onelf_format::runtime_metadata::RuntimeRuleMetadata::decode(&bytes).map_err(invalid)?;
        onelf_rt::env::carry_rules(decoded);
    }
    Ok(())
}

/// The CLI's HTTPS client, supplied as acquisition's `https://`
/// operation.
fn https_fetch(url: &str, into: &mut std::fs::File, max: u64) -> Result<(), String> {
    let response = ureq::get(url).call().map_err(|e| format!("{url}: {e}"))?;
    let copied = io::copy(
        &mut io::Read::take(response.into_body().into_reader(), max.saturating_add(1)),
        into,
    )
    .map_err(|e| format!("{url}: {e}"))?;
    if copied > max {
        return Err(format!(
            "{url}: the body is larger than the {max}-byte bound a staged build may take"
        ));
    }
    Ok(())
}

/// Run an AppDir in place: assemble a bundle launch through the shared
/// machinery when `run_bundle` is set, then exec the chosen entrypoint
/// with the environment the packed runtime's own machinery applies.
pub fn run(
    app_spec: &Path,
    command_override: Option<&str>,
    entrypoint: Option<&str>,
    run_bundle: bool,
    passthrough_args: &[String],
) -> io::Result<()> {
    // The caller's environment, captured before anything applies package
    // settings, is what the restoration record offers the host programs
    // the app spawns later.
    onelf_rt::hostenv::capture_starting_env();

    // Figure out the AppDir: user's spec is either the directory or a recipe file.
    let (dir, mut found) = resolve_app(app_spec)?;
    let dir = dir.canonicalize()?;

    // A recipe environment naming a loader search path, or a path pair a
    // runtime would refuse, is refused in every development form, before
    // anything stages or launches.
    if let Some(loaded) = &found {
        assembly::refuse_library_search_env(&loaded.path, &loaded.recipe.env)?;
        let declared: Vec<(String, String)> = loaded
            .recipe
            .paths
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        crate::pack::paths_record(&crate::pack::path_pairs(&declared))
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", loaded.path.display())))?;
    }

    let mut assembled: Option<Assembled> = None;
    if run_bundle {
        assembled = match found.take() {
            Some(loaded) => Some(assemble_for_launch(loaded)?),
            None => {
                eprintln!("onelf run: --bundle skipped: no recipe found in AppDir");
                None
            }
        };
    }

    let (ep_name, ep_path, ep_args, working_dir) = match &assembled {
        Some(a) => request_entrypoint(&a.request, command_override, entrypoint)?,
        None => resolve_entrypoint(
            found.as_ref().map(|l| &l.recipe),
            &dir,
            command_override,
            entrypoint,
        )?,
    };

    // An incomplete tree is named as such, not left to a deep error
    // from a launch against a target that does not exist.
    let target = dir.join(&ep_path);
    if !target.exists() {
        return Err(missing_target_error(
            &dir,
            &ep_path,
            found.as_ref().map(|l| &l.recipe),
        ));
    }
    if let Some(loaded) = &found {
        refuse_unacquired_sets(&loaded.recipe, &ep_name)?;
    }

    // Detect lib dirs in the AppDir (same rule as pack: any dir containing .so files,
    // excluding .onelf/, share/, bin/, etc).
    let lib_dirs = detect_lib_dirs(&dir);
    let lib_subpath = lib_dirs
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(":");
    let target_is_elf = is_elf_file_at(&target);

    // The same decision the packed runtime makes: what the host supplies is
    // chosen per library, and no host directory goes on the search path.
    // The view is this launch's own copy, so a host winner is linked over
    // the bundled copy there, as a packed launch does in a tree of its
    // own. The host's glibc never wins: a development run keeps the
    // bundled loader. An assembled launch reads the setting off the
    // request it staged, so the recipe's choice reaches the resolver in
    // bundle mode exactly as it does in a build.
    let policy = match &assembled {
        Some(a) => a.request.package.host_libs,
        None => found
            .as_ref()
            .and_then(|l| l.recipe.package.host_libs)
            .map(HostLibs::from)
            .unwrap_or(HostLibs::Auto),
    };
    let policy = match policy {
        HostLibs::Always => HostLibsPolicy::Always,
        HostLibs::Never => HostLibsPolicy::Never,
        HostLibs::Auto => HostLibsPolicy::Auto,
    };
    let disabled = std::env::var_os("ONELF_NO_RESOLVER").is_some_and(|v| !v.is_empty() && v != "0");
    let lib_dir_strs: Vec<&str> = lib_dirs.iter().filter_map(|p| p.to_str()).collect();

    // What a bundle launch acquired, through the shared launch-support
    // surface: the selected entrypoint's sets, then the pinned GL build
    // when this host lacks the stack. A set the entrypoint requires and
    // cannot have ends the launch naming the entrypoint and the pin, as
    // a packed launch does; the pinned build is warn-and-continue.
    let mut set_roots: Vec<PathBuf> = Vec::new();
    let mut set_lib_dirs: Vec<PathBuf> = Vec::new();
    let mut pinned_gl: Option<(PathBuf, std::fs::File)> = None;
    if let Some(a) = &assembled {
        let selection = selected_sets(&a.request, &ep_name)?;
        acquire_sets(&dir, selection, &ep_name)?;
        set_roots = onelf_rt::sets::roots();
        set_lib_dirs = set_roots
            .iter()
            .flat_map(|root| onelf_rt::sets::lib_dirs(root))
            .collect();
        if target_is_elf && !lib_dirs.is_empty() && !disabled && policy != HostLibsPolicy::Never {
            pinned_gl = obtain_gl_pin(&dir, &lib_dir_strs, &set_lib_dirs, &target);
        }
    }

    let resolution = match run_store(&dir) {
        Some(store) if target_is_elf && !lib_dirs.is_empty() && !disabled => {
            resolve::resolve_selected(
                &Request {
                    pkg_root: &dir,
                    lib_dirs: &lib_dir_strs,
                    plugin_dirs: &[],
                    policy,
                    store: &store,
                    ld_cache: &drivers::cache_file(),
                    icd_dirs: resolve::ICD_DIRS,
                    extra_root: pinned_gl.as_ref().map(|(root, _)| root.as_path()),
                    set_roots: &set_roots,
                    host_libc: false,
                    loader_store: resolve::LoaderStore::Refused(
                        "a development run never takes the host's glibc",
                    ),
                    nsswitch: resolve::nsswitch_file(),
                },
                &onelf_rt::sets::pins(),
            )
        }
        _ => Resolution::default(),
    };
    for name in &resolution.incomparable {
        eprintln!("onelf run: resolver: cannot order {name}; keeping the bundled copy");
    }

    // The effective links the view must show: the resolver's farm and
    // the sets, less what the tree already carries in that directory,
    // placed where the entrypoint's own run path looks first.
    let runpath_dirs = onelf_rt::links::origin_runpath(&target);
    let link_dir = onelf_rt::links::dir_where(&dir, &runpath_dirs, &lib_dir_strs, |rel| {
        dir.join(rel).is_dir()
    });
    let links = match &link_dir {
        Some(rel) => onelf_rt::links::wanted_from(
            resolution.farm.as_deref(),
            &set_roots,
            &carried_names(&dir.join(rel)),
        ),
        None => onelf_rt::links::Links::new(),
    };
    let shadows = shadowed_copies(&dir, &lib_dir_strs, resolution.farm.as_deref());

    // The launch declarations the recipe makes, read from the assembly
    // request a build would pack from when one was staged, so a
    // development launch sees exactly the declarations a build packs.
    let (recipe_env, declared_paths, declared_preload, needs_setuid) = match &assembled {
        Some(a) => (
            a.request.env.clone(),
            a.request.paths.clone(),
            a.request.preload.clone(),
            a.request.package.needs_setuid,
        ),
        None => match found.as_ref() {
            Some(l) => (
                l.recipe
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                l.recipe
                    .paths
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                l.recipe.preload.clone(),
                l.recipe.package.needs_setuid,
            ),
            None => (Vec::new(), Vec::new(), Vec::new(), false),
        },
    };
    let env_data = env_bytes(&recipe_env);

    // The runtime rule record a build of this recipe would carry, put in
    // place before any mapping is evaluated: the recipe's local rules and
    // disables, and in bundle mode the pins the assembly prepared.
    let carried = match (&assembled, &found) {
        (Some(a), _) => a.request.bundle.rules.runtime_records(&a.prepared)?,
        (None, Some(l)) => {
            let recipe_dir = l.path.parent().unwrap_or(Path::new("."));
            crate::rules::RuleCatalog::load(
                &l.path,
                recipe_dir,
                &l.recipe.bundle.rules,
                &l.recipe.bundle.disable_rules,
            )?
            .runtime_records(&[])?
        }
        (None, None) => None,
    };
    if let Some(records) = carried {
        carry_rules(records)?;
    }

    // The argv0 decision, taken once for the record pass and the launch
    // pass alike: the tree the view stages is a copy, so every path the
    // record names in AppDir form reads the same entry through the view.
    let argv0 = match target.to_str() {
        Some(path) if target.file_name() == Some(ep_name.as_ref()) => path,
        _ => ep_name.as_str(),
    };

    // Pass one, against the AppDir: apply the same machinery a packed
    // launch applies, then encode what it changed into the per-view
    // environment input. This is the generation a build would emit from
    // the same evaluated mappings, and this process ends the pass as
    // plain a CLI as it started.
    let base_env: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
    let dir_str = dir.to_string_lossy().into_owned();
    let target_str = target.to_string_lossy().into_owned();
    let _ = onelf_rt::env::setup_env(
        &dir_str,
        argv0,
        &target_str,
        &ep_name,
        "dev",
        &lib_subpath,
        &target_str,
        resolution.farm.as_deref(),
        pinned_gl.as_ref().map(|(root, _)| root.as_path()),
        &set_lib_dirs,
        &set_roots,
        &[],
    );
    onelf_rt::env::apply_custom_env(&env_data, &dir_str);
    onelf_rt::hostenv::record_host_env(&dir, &set_roots);
    let applied_env: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
    let assignments =
        onelf_rt::env::view_env_assignments(&dir, &lib_dir_strs, &set_lib_dirs, &set_roots, &[]);
    let view_record = onelf_rt::env::encode_view_env(&base_env, &applied_env, &assignments)
        .map_err(|e| io::Error::other(format!("the generated environment input is {e}")))?;
    restore_env(&base_env, &applied_env);

    // The prepared private development view: identified by everything
    // that changes what it shows or restores, built once, and reused
    // untouched until one of those inputs changes.
    let tree_id = tree_digest(&dir, &shadows)?;
    let pins = onelf_rt::sets::pins();
    let inputs = onelf_rt::links::view::ViewInputs {
        uid: rustix::process::getuid().as_raw(),
        package: &tree_id,
        metadata: None,
        pins: &pins,
        policy,
        host_libc: false,
        loader: resolution.host_interp.as_ref().map(|l| l.path.as_os_str()),
        entrypoint: Some(ep_name.as_str()),
        links: &links,
        runtime_env: &view_record,
        root: dir.as_os_str(),
    };
    let identity = onelf_rt::links::view::ViewIdentity::new(&inputs).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "this launch's generated environment input is beyond what a development view records",
        )
    })?;

    let store = view_store()?;
    let view_name = onelf_rt::links::view::dir_name(identity.key()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the view identity names no directory",
        )
    })?;
    let view = store.join(view_name);
    let lease_path = onelf_rt::links::view::lease_lock_path(&view)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the view names no lease"))?;
    let lease = onelf_rt::paths::open_lock_inheritable(&lease_path)?;
    rustix::fs::flock(&lease, rustix::fs::FlockOperation::LockShared)
        .map_err(|e| io::Error::other(format!("the view lease: {e}")))?;

    let path_pairs = crate::pack::path_pairs(&declared_paths);
    let published = onelf_rt::links::view::ensure_view(&store, &identity, &lease, |staging| {
        stage_tree(staging, &dir)?;
        for (rel, host) in &shadows {
            let staged = staging.dir().join(rel);
            std::fs::remove_file(&staged)?;
            std::os::unix::fs::symlink(host, &staged)?;
        }
        let expanded = expand_view_roots(&view_record, dir.as_os_str(), view.as_os_str());
        staging.write_file(onelf_rt::env::VIEW_ENV_FILE, &expanded, 0o644)?;
        if let Some(rel) = &link_dir {
            staging.place_free_links(rel, &links)?;
        }
        if needs_setuid {
            staging.write_file(".onelf/needs-setuid", b"1\n", 0o644)?;
        }
        Ok(Some(expanded))
    })?;

    // Pass two, against the view: the same machinery, applied to the
    // prepared tree the entrypoint actually runs from.
    let view_root = published.dir().to_path_buf();
    let view_str = view_root.to_string_lossy().into_owned();
    let view_target = view_root.join(&ep_path);
    let view_target_str = view_target.to_string_lossy().into_owned();
    let lib_path = onelf_rt::env::setup_env(
        &view_str,
        argv0,
        &view_target_str,
        &ep_name,
        "dev",
        &lib_subpath,
        &view_target_str,
        resolution.farm.as_deref(),
        pinned_gl.as_ref().map(|(root, _)| root.as_path()),
        &set_lib_dirs,
        &set_roots,
        &[],
    );
    onelf_rt::env::apply_custom_env(&env_data, &view_str);
    onelf_rt::hostenv::record_host_env(&view_root, &set_roots);

    // Capability diagnostics, stated outright: a development launch
    // carries no packaged constructor, so the recipe's preload list is
    // not opened, and only the initial environment is prepared, so a
    // re-exec that clears the environment starts outside the prepared
    // view. Neither is promised silently.
    if !declared_preload.is_empty() {
        eprintln!(
            "onelf run: the recipe's preload list is opened by the packaged constructor, \
             which a development launch does not carry"
        );
    }
    eprintln!(
        "onelf run: a development launch prepares the initial environment only; \
         a re-exec that clears the environment is not hooked"
    );
    if !declared_paths.is_empty() {
        if onelf_rt::hostpaths::apply(&view_root, &path_pairs, &std::collections::BTreeMap::new()) {
            eprintln!(
                "onelf run: the view's directories answer for the absolute paths the recipe built with"
            );
        } else {
            eprintln!(
                "onelf run: the recipe's absolute paths are not overlaid: this launch could not \
                 make the view's directories answer for them, so the application reads the host's"
            );
        }
    }

    let requested_cwd = match working_dir {
        WorkingDir::PackageRoot => Some(view_root.clone()),
        WorkingDir::EntrypointParent => view_target.parent().map(Path::to_path_buf),
        WorkingDir::Inherit => None,
    };

    // The view carries the farm and set links in its library directory,
    // as a packed tree does, so the packed launch's gate applies as is.
    let kernel_lib_path =
        onelf_rt::launch::kernel_library_path(&view_root, &lib_dir_strs, &lib_path, &runpath_dirs);

    let mut cmd = build_exec_command(
        &view_target,
        &view_root,
        &lib_path,
        kernel_lib_path.as_deref(),
        argv0,
        resolution.host_interp.as_ref().map(|l| l.path.as_path()),
    )?;
    cmd.args(&ep_args);
    cmd.args(passthrough_args);

    if let Some(c) = requested_cwd {
        cmd.env(
            "ONELF_USER_CWD",
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        );
        cmd.current_dir(c);
    }

    // The lease descriptor is inheritable, so leaking it here keeps the
    // shared lock held by the entrypoint across the exec: the view backs
    // a live process the moment the entrypoint starts.
    let _ = std::os::fd::IntoRawFd::into_raw_fd(lease);

    // exec never returns on success.
    let err = cmd.exec();
    Err(io::Error::other(format!("exec failed: {err}")))
}

/// The recipe's `[env]` table as the constructor reads it, keys in the
/// sorted order the emitted record keeps.
fn env_bytes(env: &[(String, String)]) -> Vec<u8> {
    let mut data = Vec::new();
    for (key, value) in env {
        data.extend_from_slice(format!("{key}={value}\n").as_bytes());
    }
    data
}

/// The names the tree itself has in `dir`, a path inside the AppDir:
/// everything staged there under its own name, of any kind, so nothing
/// of the tree is shadowed or taken for a link a launch placed.
fn carried_names(dir: &Path) -> HashSet<String> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .collect()
}

/// Undo one application pass: every variable the pass took from the
/// caller is given back, every one it added is removed.
///
/// The pass ran the shared machinery on this process, as a packed launch
/// does on the payload's process; a development run has more CLI work
/// after it, and its own rules apply to that, not the entrypoint's. This
/// runs single-threaded, before any thread exists to observe the
/// intermediate states.
fn restore_env(base: &BTreeMap<OsString, OsString>, applied: &BTreeMap<OsString, OsString>) {
    for (name, value) in base {
        if applied.get(name) != Some(value) {
            // SAFETY: single-threaded, between the two application passes
            unsafe { std::env::set_var(name, value) };
        }
    }
    for name in applied.keys() {
        if !base.contains_key(name) {
            // SAFETY: single-threaded, between the two application passes
            unsafe { std::env::remove_var(name) };
        }
    }
}

/// The host winners the resolver chose over copies the tree carries:
/// for each farm entry a library directory of the tree resolves to a
/// file, that name's path relative to the tree and the host file the farm
/// names. A packed launch whose tree is its own replaces each such copy
/// with a link to the host file, and the development view, which is its
/// launch's own copy, does the same.
fn shadowed_copies(dir: &Path, lib_dirs: &[&str], farm: Option<&Path>) -> Vec<(String, PathBuf)> {
    let Some(entries) = farm.and_then(|farm| std::fs::read_dir(farm).ok()) else {
        return Vec::new();
    };
    let mut shadows = Vec::new();
    for entry in entries.flatten() {
        let (Ok(host), Some(name)) = (
            std::fs::read_link(entry.path()),
            entry.file_name().to_str().map(String::from),
        ) else {
            continue;
        };
        for lib in lib_dirs {
            let rel = format!("{lib}/{name}");
            if dir.join(&rel).is_file() {
                shadows.push((rel, host.clone()));
            }
        }
    }
    shadows.sort();
    shadows
}

/// The development view's content identity: a digest over every tree
/// entry's relative path, kind, link target where one exists, and, for
/// regular files, mode, size, timestamps and device and inode numbers,
/// then over the host files `shadows` places over bundled copies. A
/// tree whose content changed in any way, or a changed host winner,
/// digests differently; a tree that only moved keeps its digest.
fn tree_digest(root: &Path, shadows: &[(String, PathBuf)]) -> io::Result<[u8; 32]> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let mut hasher = blake3::Hasher::new();
    for (rel, host) in shadows {
        hasher.update(b"s");
        hasher.update(rel.as_bytes());
        hasher.update(b"\0");
        hasher.update(host.as_os_str().as_encoded_bytes());
        hasher.update(b"\0");
    }
    for entry in jwalk::WalkDir::new(root).skip_hidden(false).sort(true) {
        let entry = entry.map_err(|e| io::Error::other(e.to_string()))?;
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .map_err(|_| io::Error::other("the staged tree escapes its root"))?;
        hasher.update(rel.as_os_str().as_encoded_bytes());
        hasher.update(b"\0");
        let kind = entry.file_type();
        if kind.is_dir() {
            hasher.update(b"d");
        } else if kind.is_symlink() {
            hasher.update(b"l");
            let target = std::fs::read_link(entry.path())?;
            hasher.update(target.as_os_str().as_encoded_bytes());
            hasher.update(b"\0");
        } else if kind.is_file() {
            hasher.update(b"f");
            let md = std::fs::symlink_metadata(entry.path())?;
            hasher.update(&md.permissions().mode().to_le_bytes());
            hasher.update(&md.len().to_le_bytes());
            hasher.update(&md.mtime().to_le_bytes());
            hasher.update(&md.mtime_nsec().to_le_bytes());
            hasher.update(&md.dev().to_le_bytes());
            hasher.update(&md.ino().to_le_bytes());
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{}: not a regular file, directory or symlink",
                    entry.path().display()
                ),
            ));
        }
    }
    Ok(*hasher.finalize().as_bytes())
}

/// Copy the development tree into `staging` for publication: directories
/// created, files cloned in as their own inodes, symlinks remade inside
/// the view, and anything else refused with the entry named.
fn stage_tree(staging: &onelf_rt::links::view::Staging, root: &Path) -> io::Result<()> {
    for entry in jwalk::WalkDir::new(root).skip_hidden(false).sort(true) {
        let entry = entry.map_err(|e| io::Error::other(e.to_string()))?;
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .map_err(|_| io::Error::other("the staged tree escapes its root"))?;
        let rel = rel.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{}: not a UTF-8 path", path.display()),
            )
        })?;
        if rel.is_empty() {
            continue;
        }
        let kind = entry.file_type();
        if kind.is_dir() {
            std::fs::create_dir_all(staging.dir().join(rel))?;
        } else if kind.is_symlink() {
            let target = std::fs::read_link(entry.path())?;
            let target = target.to_str().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{}: not a UTF-8 symlink target", entry.path().display()),
                )
            })?;
            staging.symlink(rel, target)?;
        } else if kind.is_file() {
            clone_file(&entry.path(), &staging.dir().join(rel))?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{}: not a regular file, directory or symlink",
                    entry.path().display()
                ),
            ));
        }
    }
    Ok(())
}

/// Give `dest` the bytes and permission bits of the regular file at
/// `source` as a new inode: a reflink where the filesystem shares
/// extents, a plain copy otherwise. Never a hardlink, so an edit to the
/// AppDir file cannot reach a published view through a shared inode.
fn clone_file(source: &Path, dest: &Path) -> io::Result<()> {
    let from = std::fs::File::open(source)?;
    let mut to = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)?;
    if rustix::fs::ioctl_ficlone(&to, &from).is_err() {
        io::copy(&mut &from, &mut to)?;
    }
    to.set_permissions(from.metadata()?.permissions())
}

/// The environment input as it is finally written into the view: every
/// occurrence of the AppDir replaced by the view that stages the same
/// entries, the way a packed launch's record names its own tree.
fn expand_view_roots(bytes: &[u8], from: &OsStr, to: &OsStr) -> Vec<u8> {
    let (from, to) = (from.as_encoded_bytes(), to.as_encoded_bytes());
    if from.is_empty() || from == to {
        return bytes.to_vec();
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    while let Some(at) = rest.windows(from.len()).position(|w| w == from) {
        out.extend_from_slice(&rest[..at]);
        out.extend_from_slice(to);
        rest = &rest[at + from.len()..];
    }
    out.extend_from_slice(rest);
    out
}

/// The private store of prepared development views, under the cache root
/// the packed forms share and created `0700`: every view carries a
/// record of the caller's environment, so nothing here is shared.
fn view_store() -> io::Result<PathBuf> {
    let root = layout::resolve_root(rustix::process::getuid().as_raw(), None).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no safe cache directory (set XDG_CACHE_HOME or HOME)",
        )
    })?;
    let store = root.join("views");
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::DirBuilder::new().mode(0o700).create(&store) {
        Ok(()) => Ok(store),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(store),
        Err(e) => Err(e),
    }
}

/// The entrypoint target's error for a directory that is not a prepared
/// tree: what is missing, and the assembly that would have staged it.
fn missing_target_error(dir: &Path, ep_path: &str, staged: Option<&recipe::Recipe>) -> io::Error {
    let target = dir.join(ep_path);
    let stages = staged.is_some_and(|r| r.input.is_some() || r.sysroot.is_some());
    let hint = if stages {
        " (the recipe stages its contents: run with --bundle, or build first)"
    } else {
        ""
    };
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("entrypoint target not found: {}{hint}", target.display()),
    )
}

/// Refuse a launch without `--bundle` whose entrypoint selects declared
/// sets. Only an assembled launch acquires them, and without them the
/// resolver would take what they provide from the host, which would
/// conceal an incomplete tree instead of reporting it.
fn refuse_unacquired_sets(recipe: &recipe::Recipe, ep_name: &str) -> io::Result<()> {
    let selections = recipe::SetSelections::normalize(recipe)?;
    let selected: Vec<&str> = match selections.named.get(ep_name).unwrap_or(&selections.primary) {
        recipe::SetSelection::All => recipe
            .sysroot
            .iter()
            .flat_map(|sr| sr.sets.keys().map(String::as_str))
            .collect(),
        recipe::SetSelection::Names(names) => names.iter().map(String::as_str).collect(),
    };
    if selected.is_empty() {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "entrypoint '{ep_name}' selects the sets {}, which only an assembled launch \
             acquires; without them their libraries would come from the host. Run with \
             --bundle, or build the package",
            selected.join(", ")
        ),
    ))
}

/// Pick the entrypoint an assembled launch runs: the `--command`
/// override, the named entrypoint, the recipe's default entrypoint, or
/// the package command, in that order. The named entrypoint the caller
/// asks for is the one that launches, with its own path and arguments.
fn request_entrypoint(
    request: &assembly::AssemblyRequest,
    command_override: Option<&str>,
    entrypoint: Option<&str>,
) -> io::Result<(String, String, Vec<String>, WorkingDir)> {
    let working_dir = request.package.working_dir;

    if let Some(cmd) = command_override {
        let name = Path::new(cmd)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("app")
            .to_string();
        return Ok((name, cmd.to_string(), Vec::new(), working_dir));
    }

    if let Some(name) = entrypoint {
        let ep = request
            .entrypoints
            .iter()
            .find(|e| e.name == name)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("entrypoint '{name}' not defined in recipe"),
                )
            })?;
        return Ok((
            ep.name.clone(),
            ep.path.clone(),
            ep.args.clone(),
            working_dir,
        ));
    }

    if let Some(default) = &request.default_entrypoint
        && let Some(ep) = request.entrypoints.iter().find(|e| &e.name == default)
    {
        return Ok((
            ep.name.clone(),
            ep.path.clone(),
            ep.args.clone(),
            working_dir,
        ));
    }

    let name = request.package.name.clone().unwrap_or_else(|| {
        request
            .package
            .command
            .rsplit('/')
            .next()
            .unwrap_or("app")
            .to_string()
    });
    Ok((
        name,
        request.package.command.clone(),
        Vec::new(),
        working_dir,
    ))
}

/// The set selection the chosen entrypoint launches with, read from the
/// resolved selection table the assembly carried: the row naming the
/// chosen entrypoint, or the default row for a `--command` override or
/// the package command. Refusals a build raises over that table, a
/// duplicate name or a row without a declared selection, ended the
/// assembly before this launch got here.
fn selected_sets(
    request: &assembly::AssemblyRequest,
    ep_name: &str,
) -> io::Result<recipe::SetSelection> {
    let row = request
        .selections
        .iter()
        .find(|row| row.name == ep_name)
        .unwrap_or(&request.selections[0]);
    Ok(row.sets.clone())
}

/// Acquire the selected entrypoint's sets from the AppDir's record,
/// through the shared acquisition whose pins and whose provider checks a
/// packed launch uses. An entrypoint whose selection names nothing
/// acquires nothing.
fn acquire_sets(dir: &Path, selection: recipe::SetSelection, ep_name: &str) -> io::Result<()> {
    let pins = match std::fs::read_to_string(dir.join(onelf_rt::sets::SETS_FILE)) {
        Ok(text) => onelf_rt::sets::parse(&text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let selected = match selection {
        recipe::SetSelection::All => pins,
        recipe::SetSelection::Names(names) => {
            let mut out = Vec::with_capacity(names.len());
            for name in names {
                let found = pins.iter().find(|p| p.label == name).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("entrypoint '{ep_name}': set '{name}' is not declared"),
                    )
                })?;
                out.push(found.clone());
            }
            out
        }
    };
    onelf_rt::sets::init(&selected, ep_name);
    Ok(())
}

/// Obtain the GL build the AppDir pins, when this host lacks a GL stack
/// the launch would otherwise have to miss. The same situation a packed
/// launch resolves, with the same warn-and-continue policy: anything
/// that stops the pinned build is a warning, and the launch goes on
/// without a GL stack. Returns the extracted root and the lock that
/// keeps the extraction from being collected while this instance runs.
fn obtain_gl_pin(
    dir: &Path,
    lib_dirs: &[&str],
    set_lib_dirs: &[PathBuf],
    target: &Path,
) -> Option<(PathBuf, std::fs::File)> {
    let situation = onelf_format::resolve::gl_situation(
        dir,
        lib_dirs,
        set_lib_dirs,
        Some(target),
        &drivers::cache_file(),
        resolve::ICD_DIRS,
    );
    if !situation.lacking() {
        return None;
    }
    match onelf_rt::platform::obtain(dir) {
        Some(Ok(fetched)) => Some(fetched),
        Some(Err(why)) => {
            eprintln!(
                "onelf run: this host has no GL stack and the pinned build cannot be used \
                 ({why}); continuing without one"
            );
            None
        }
        None => {
            if situation == Gl::Missing {
                eprintln!(
                    "onelf run: this host has no GL stack, which the entrypoint links, and \
                     the recipe pins no GL build; continuing without one"
                );
            }
            None
        }
    }
}

/// Build a Command for executing the target binary. Handles the cross-libc
/// case where the target's PT_INTERP doesn't exist on the host but a matching
/// interpreter is bundled in the AppDir's lib directories.
/// Locate the AppDir root and its recipe from a user-supplied path.
/// Accepts a directory (with or without onelf.toml) or a `.toml` file.
fn resolve_app(spec: &Path) -> io::Result<(PathBuf, Option<Loaded>)> {
    if spec.is_dir() {
        let toml = spec.join("onelf.toml");
        let loaded = toml
            .is_file()
            .then(|| recipe::load(&toml).map(|recipe| Loaded { path: toml, recipe }))
            .transpose()?;
        return Ok((spec.to_path_buf(), loaded));
    }
    if spec.is_file() && spec.extension().and_then(|s| s.to_str()) == Some("toml") {
        let recipe = recipe::load(spec)?;
        let dir = spec
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        return Ok((
            dir,
            Some(Loaded {
                path: spec.to_path_buf(),
                recipe,
            }),
        ));
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "{}: expected an AppDir (directory) or an onelf.toml file",
            spec.display()
        ),
    ))
}

/// Pick the entrypoint and its args for a launch that skips assembly,
/// consulting (in order): `--command` override, the recipe's named
/// entrypoints, the recipe's default command, or an auto-detected binary
/// under `<dir>/bin/`.
fn resolve_entrypoint(
    recipe: Option<&recipe::Recipe>,
    dir: &Path,
    command_override: Option<&str>,
    entrypoint: Option<&str>,
) -> io::Result<(String, String, Vec<String>, WorkingDir)> {
    let working_dir: WorkingDir = recipe
        .map(|r| r.package.working_dir.into())
        .unwrap_or(WorkingDir::Inherit);

    if let Some(cmd) = command_override {
        let name = Path::new(cmd)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("app")
            .to_string();
        return Ok((name, cmd.to_string(), Vec::new(), working_dir));
    }

    if let Some(r) = recipe {
        if let Some(name) = entrypoint {
            let ep = r
                .entrypoint
                .iter()
                .find(|e| e.name == name)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("entrypoint '{name}' not defined in recipe"),
                    )
                })?;
            return Ok((
                ep.name.clone(),
                ep.path.clone(),
                ep.args.clone(),
                working_dir,
            ));
        }
        if let Some(ep) = r.entrypoint.iter().find(|e| e.default) {
            return Ok((
                ep.name.clone(),
                ep.path.clone(),
                ep.args.clone(),
                working_dir,
            ));
        }
        let name = r.package.name.clone().unwrap_or_else(|| {
            r.package
                .command
                .rsplit('/')
                .next()
                .unwrap_or("app")
                .to_string()
        });
        return Ok((name, r.package.command.clone(), Vec::new(), working_dir));
    }

    if entrypoint.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--entrypoint requires an onelf.toml in the AppDir",
        ));
    }

    // No recipe, no override: find a single executable under bin/.
    let bin = dir.join("bin");
    if !bin.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{}: no onelf.toml and no bin/ subdirectory; pass --command PATH",
                dir.display()
            ),
        ));
    }
    let entries: Vec<PathBuf> = std::fs::read_dir(&bin)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    if entries.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{}: no executables found", bin.display()),
        ));
    }
    if entries.len() > 1 {
        let names: Vec<String> = entries
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(String::from))
            .collect();
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{}: multiple binaries found; pass --command bin/NAME (have: {})",
                bin.display(),
                names.join(", ")
            ),
        ));
    }
    let bin_path = &entries[0];
    let name = bin_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("app")
        .to_string();
    let rel = format!("bin/{name}");
    Ok((name, rel, Vec::new(), working_dir))
}

/// Result of choosing how to exec the target.
///
/// When the target's PT_INTERP was patched to a relative path that
/// resolves under `app_dir` (i.e. `bundle-libs` did its job), we can
/// execve the target directly and let the kernel load the bundled ld.
/// That keeps `/proc/self/exe` pointing at the real binary, which is
/// what Python, Electron and friends read to find their resources.
///
/// Falls back to invoking the bundled loader explicitly when we have
/// one but PT_INTERP wasn't patched, or to a bare exec if nothing is
/// bundled (the host must have the right loader).
///
/// A bare kernel exec is given `kernel_lib_path` as `LD_LIBRARY_PATH` on
/// the command, the same fallback a packed launch applies: the caller
/// decides when the target's own metadata cannot reach what the launch
/// resolved, and the caller's inherited value is never part of what is
/// set.
fn build_exec_command(
    target: &Path,
    app_dir: &Path,
    lib_path: &str,
    kernel_lib_path: Option<&str>,
    argv0: &str,
    host_interp: Option<&Path>,
) -> io::Result<Command> {
    let interp = read_elf_interp(target);

    // The host's glibc won, so the bundled loader must not run at all. A
    // bootstrap-injected binary carries no PT_INTERP and maps its loader
    // itself; ONELF_INTERP redirects it, and every re-exec inherits that.
    // Anything else is handed to the host loader directly.
    if let Some(host_interp) = host_interp {
        if interp.is_none() {
            let mut cmd = Command::new(target);
            cmd.arg0(argv0).env("ONELF_INTERP", host_interp);
            if let Some(path) = &kernel_lib_path {
                cmd.env("LD_LIBRARY_PATH", path);
            }
            return Ok(cmd);
        }
        let mut cmd = Command::new(host_interp);
        cmd.arg("--inhibit-cache");
        if !lib_path.is_empty() {
            cmd.arg("--library-path").arg(lib_path);
        }
        cmd.arg("--argv0").arg(argv0).arg(target);
        return Ok(cmd);
    }

    if let Some(interp) = interp {
        let interp_path = Path::new(&interp);

        if let Some(bundled) = find_bundled_interp(&interp, app_dir) {
            let is_musl = interp_path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("ld-musl-"));
            let mut cmd = Command::new(&bundled);
            if !is_musl {
                cmd.arg("--inhibit-cache");
            }
            if !lib_path.is_empty() {
                cmd.arg("--library-path").arg(lib_path);
            }
            cmd.arg("--argv0").arg(argv0).arg(target);
            return Ok(cmd);
        }

        if !interp_path.exists() {
            eprintln!(
                "warning: ELF interpreter {} not found on host and no bundled \
                 equivalent in the AppDir; exec will likely fail",
                interp
            );
        }
    }

    let mut cmd = Command::new(target);
    cmd.arg0(argv0);
    if let Some(path) = &kernel_lib_path {
        cmd.env("LD_LIBRARY_PATH", path);
    }
    Ok(cmd)
}

/// Read PT_INTERP from an ELF file. Returns None for non-ELF or missing interp.
/// Strips trailing NULs introduced by our PT_INTERP patching, which pads the
/// slot with zero bytes rather than shrinking p_filesz.
fn read_elf_interp(path: &Path) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    goblin::elf::Elf::parse(&data)
        .ok()?
        .interpreter
        .map(|s| s.trim_end_matches('\0').to_string())
}

/// Look in the AppDir's lib dirs for a file matching the PT_INTERP basename.
fn find_bundled_interp(interp: &str, app_dir: &Path) -> Option<PathBuf> {
    let name = Path::new(interp).file_name()?.to_os_string();
    for rel in detect_lib_dirs(app_dir) {
        let candidate = app_dir.join(&rel).join(&name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// True if `path` is an ELF file (first four bytes `\x7fELF`). Scripts
/// (shebang `#!`) and missing files return false. Used to decide which
/// launch machinery applies: the resolver and the search path serve ELF
/// targets, while a script is handed to a host interpreter linked
/// against the host's glibc, which must never see the bundled libc.
fn is_elf_file_at(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 4];
    matches!(f.read(&mut buf), Ok(4)) && buf == *b"\x7fELF"
}

/// Where the resolver keeps its link farm and recorded decision for the
/// AppDir at `dir`: under the cache root, keyed by the directory's path,
/// so repeated dev runs of the same tree reuse it.
fn run_store(dir: &Path) -> Option<PathBuf> {
    let root = layout::resolve_root(rustix::process::getuid().as_raw(), None)?;
    let id = blake3::hash(dir.as_os_str().as_encoded_bytes()).to_hex();
    Some(root.join("run").join(&id[..16]))
}

/// Find subdirectories of `dir` that contain `.so*` files, skipping obvious
/// non-lib roots. Mirrors the heuristic in pack.rs's auto_detect_lib_dirs.
fn detect_lib_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut lib_dirs: Vec<PathBuf> = Vec::new();
    for entry in jwalk::WalkDir::new(dir).skip_hidden(false).sort(true) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.contains(".so") {
            continue;
        }
        let Some(parent) = path.parent() else {
            continue;
        };
        let Ok(rel) = parent.strip_prefix(dir) else {
            continue;
        };
        let rel_str = rel.to_string_lossy();
        if rel_str.is_empty()
            || rel_str.starts_with(".onelf")
            || rel_str.starts_with("share/")
            || rel_str == "bin"
            || rel_str.starts_with("bin/")
            || rel_str.starts_with("etc/")
        {
            continue;
        }
        let rel_path = rel.to_path_buf();
        if !lib_dirs.contains(&rel_path) {
            lib_dirs.push(rel_path);
        }
    }
    lib_dirs
}

#[cfg(test)]
mod selection_tests {
    use super::selected_sets;
    use crate::assembly::AssemblyRequest;
    use crate::recipe::{self, SetSelection};

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// A temp directory unique per test, so parallel tests never share
    /// state.
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "onelf-run-selection-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Plan an assembly request from a recipe body; the request carries
    /// the resolved selection table.
    fn request(dir: &std::path::Path, body: &str) -> AssemblyRequest {
        let path = dir.join("onelf.toml");
        std::fs::write(&path, body).unwrap();
        let recipe = recipe::load(&path).unwrap();
        AssemblyRequest::plan(&path, recipe, None).unwrap()
    }

    #[test]
    fn selection_scopes_resolve_like_a_builds_table() {
        let dir = temp_dir("scopes");
        let body = format!(
            "[package]\ncommand = \"bin/app\"\n\n[bundle]\nskip = true\n\n\
             [sysroot.sets.toolkit]\npackages = [\"t\"]\nurl = \"https://e/t.onelf\"\n\
             blake3 = \"{HASH}\"\n\n\
             [[entrypoint]]\nname = \"cli\"\npath = \"bin/app\"\nsets = []\ndefault = true\n\n\
             [[entrypoint]]\nname = \"gui\"\npath = \"bin/app\"\nsets = [\"toolkit\"]\n"
        );
        let request = request(&dir, &body);

        assert_eq!(
            selected_sets(&request, "gui").unwrap(),
            SetSelection::Names(vec!["toolkit".to_string()])
        );
        assert_eq!(
            selected_sets(&request, "cli").unwrap(),
            SetSelection::Names(Vec::new())
        );
        // A --command override names no table row, so the default row's
        // selection applies.
        assert_eq!(
            selected_sets(&request, "elsewhere").unwrap(),
            SetSelection::Names(Vec::new())
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_package_command_carries_the_primary_selection() {
        let dir = temp_dir("primary");
        let body = format!(
            "[package]\ncommand = \"bin/app\"\n\n\
             [sysroot.sets.toolkit]\npackages = [\"t\"]\nurl = \"https://e/t.onelf\"\n\
             blake3 = \"{HASH}\"\n"
        );
        let request = request(&dir, &body);
        assert_eq!(selected_sets(&request, "app").unwrap(), SetSelection::All);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

//! `--onelf-doctor`: what a launch of this package would do on this host,
//! worked out without running the app and without acquiring anything.
//!
//! The report explains the carried metadata and its capability
//! requirements, the entrypoint's selected dependency sets, and the
//! execution-view identity a launch would publish, beside the mode
//! verdicts. It reads only package records and host facts: no set is
//! fetched, no store or cache is written, no view is published, and
//! paths derived from environment values are described, never printed.
//!
//! The mode rules repeat the order `main` tries them in. The resolver needs
//! the bundled libraries on disk, so the library directories are extracted
//! to a scratch directory that is removed again before the report ends.

use std::path::Path;

use onelf_format::elf::{self, RuntimeNote, RuntimeRequirements};
use onelf_format::paths::PathPair;
use onelf_format::resolve::{self, Gl, Request};
use onelf_format::runtime_metadata::EntrypointSets;
use onelf_format::verdef::Choice;
use onelf_format::{HostLibsPolicy, drivers};

use onelf_rt::cache;
use onelf_rt::fuse::mount;
use onelf_rt::loader::PackageData;
use onelf_rt::loader::read_package_file;
use onelf_rt::pkgmeta;
use onelf_rt::platform::Pin;

fn env_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty() && v != "0")
}

/// The report never repeats a control byte it was handed: names and
/// reasons a host module carries must not forge report lines.
fn plain(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// Why a launch's pairs would be left unredirected, in the order the
/// runtime refuses them: no mode to run in, a memfd launch's missing
/// tree, the package's own setuid request, the environment's opt-out,
/// the host's restriction.
fn pairs_unredirected(chosen: Option<&str>, needs_setuid: bool, ns: bool) -> Option<String> {
    if chosen.is_none() {
        Some("no execution mode can run this package here".to_string())
    } else if chosen == Some("memfd") {
        Some("the entrypoint runs from memory, which carries no package tree".to_string())
    } else if needs_setuid {
        Some(SETUID_REFUSAL.to_string())
    } else if env_set("ONELF_FUSE_NO_NAMESPACE") {
        Some(FUSE_NO_NAMESPACE_REFUSAL.to_string())
    } else if !ns {
        Some(NS_RESTRICTED.to_string())
    } else {
        None
    }
}

/// Why a launch stays out of a user namespace of its own, as the
/// runtime's overlay refuses it: the package's own setuid request, the
/// environment's opt-out, then the host's capability. The same three
/// strings the mode verdicts use.
const SETUID_REFUSAL: &str = "the package runs setuid binaries, which a user namespace disables";
const NS_RESTRICTED: &str = "user namespaces are restricted on this host";
const FUSE_NO_NAMESPACE_REFUSAL: &str = "ONELF_FUSE_NO_NAMESPACE is set";

/// The most symlink entries one carried-side lookup follows, a bound
/// well short of the kernel's own resolution limit, so a crafted
/// chain or cycle ends the lookup instead of running it forever.
const SYMLINK_HOPS: usize = 8;

/// Whether the manifest carries `relative` as a directory a launch's
/// overlay could bind: the lookup walks the manifest one component at
/// a time the way the kernel resolves the extracted tree. A directory
/// entry descends; a symlink entry queues its target's components
/// ahead of the rest and walks on from the link's directory, so a
/// multi-component target resolves through intermediate links; `..`
/// ascends one component, and the package root is a real resolution;
/// a file entry, a missing component, or an ascent above the root
/// binds nothing. Symlink hops are bounded.
fn carried_directory(
    kinds: &std::collections::HashMap<String, onelf_format::EntryKind>,
    links: &std::collections::HashMap<String, String>,
    relative: &str,
) -> bool {
    use std::collections::VecDeque;
    let mut hops = 0usize;
    let mut pending: VecDeque<&str> = relative.split('/').collect();
    let mut walked = String::new();
    while let Some(part) = pending.pop_front() {
        match part {
            "" | "." => {}
            ".." => {
                walked = match walked.rsplit_once('/') {
                    Some((parent, _)) => parent.to_string(),
                    None if !walked.is_empty() => String::new(),
                    None => return false,
                };
            }
            part => {
                if !walked.is_empty() {
                    walked.push('/');
                }
                walked.push_str(part);
                match kinds.get(&walked) {
                    Some(onelf_format::EntryKind::Dir) => {}
                    Some(onelf_format::EntryKind::Symlink) => {
                        hops += 1;
                        if hops > SYMLINK_HOPS {
                            return false;
                        }
                        let Some(target) = links.get(&walked) else {
                            return false;
                        };
                        if target.is_empty() || target.starts_with('/') {
                            return false;
                        }
                        for component in target.split('/').rev() {
                            pending.push_front(component);
                        }
                        walked = match walked.rsplit_once('/') {
                            Some((parent, _)) => parent.to_string(),
                            None => String::new(),
                        };
                    }
                    _ => return false,
                }
            }
        }
    }
    walked.is_empty() || kinds.get(&walked) == Some(&onelf_format::EntryKind::Dir)
}

/// Append one report line.
fn line(out: &mut String, text: &str) {
    out.push_str(text);
    out.push('\n');
}

/// What a mode would do, and whether it could put a host file over a
/// bundled one: only a private namespace or a tree of its own can.
type Verdict = Result<(String, bool), String>;

/// The modes a launch tries, in order, each with what it would do or why
/// it would be passed over. The list ends at the first that would run.
fn modes(
    pkg: &PackageData,
    ep_idx: usize,
    needs_setuid: bool,
    ns: bool,
) -> Vec<(&'static str, Verdict)> {
    let forced = std::env::var("ONELF_MODE").ok();
    let cache_preferred = forced.is_none()
        && pkg
            .footer
            .flags
            .contains(onelf_format::Flags::CACHE_PREFERRED);
    let dev_fuse = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
        .map(drop);
    let helper = mount::helper();
    let needed = onelf_rt::ephemeral::tmpfs_size_for(pkg);

    let no_ns = if needs_setuid {
        Some(SETUID_REFUSAL.to_string())
    } else if !ns {
        Some(NS_RESTRICTED.to_string())
    } else {
        None
    };
    let fuse = || {
        let why_not_ns = no_ns.clone().or_else(|| {
            if env_set("ONELF_FUSE_NO_NAMESPACE") {
                Some(FUSE_NO_NAMESPACE_REFUSAL.to_string())
            } else {
                dev_fuse
                    .as_ref()
                    .err()
                    .map(|e| format!("/dev/fuse cannot be opened ({e})"))
            }
        });
        match (why_not_ns, &helper) {
            (None, _) => Ok(("mounted in a private user namespace".to_string(), true)),
            (Some(why), Some(helper)) => Ok((
                format!(
                    "mounted through {} ({why})",
                    plain(&helper.display().to_string())
                ),
                false,
            )),
            (Some(why), None) => Err(format!("{why}, and there is no mount helper")),
        }
    };
    let tmpfs = || match &no_ns {
        Some(why) => Err(why.clone()),
        None if !onelf_rt::ephemeral::fits_in_memory(needed) => Err(format!(
            "the {} MB package does not fit in available memory",
            needed >> 20
        )),
        None => Ok((
            "extracted to a tmpfs in a private user namespace".to_string(),
            true,
        )),
    };
    let rundir = || match onelf_rt::paths::private_dir() {
        None => Err("there is no private runtime directory".to_string()),
        Some(dir) => match onelf_rt::rundir::free_space(&dir) {
            Some(free) if free < needed => Err(format!(
                "the private runtime directory has {} MB free, the package needs {} MB",
                free >> 20,
                needed >> 20
            )),
            _ => Ok((
                "extracted to the private runtime directory".to_string(),
                true,
            )),
        },
    };
    let cache = || {
        let requested = pkg
            .footer
            .flags
            .contains(onelf_format::Flags::CACHE_REQUESTED)
            || env_set("ONELF_CACHE");
        if requested || forced.as_deref() == Some("cache") {
            Ok(("extracted to the persistent cache".to_string(), false))
        } else {
            Err("not allowed; ONELF_CACHE=1 allows it".to_string())
        }
    };
    let memfd = || {
        if cache_preferred {
            Err("the package prefers the persistent cache".to_string())
        } else if onelf_rt::anchor::carried(pkg) {
            Err("the package anchors directories, which need a package tree".to_string())
        } else if pkg.manifest.entrypoints[ep_idx].is_memfd_eligible() {
            Ok(("the static entrypoint runs from memory".to_string(), false))
        } else {
            Err("the entrypoint needs shared libraries".to_string())
        }
    };

    let mut tries: [(&'static str, &dyn Fn() -> Verdict); 5] = [
        ("memfd", &memfd),
        ("fuse", &fuse),
        ("tmpfs", &tmpfs),
        ("rundir", &rundir),
        ("cache", &cache),
    ];
    if cache_preferred {
        tries.rotate_right(1);
    }
    let mut out = Vec::new();
    for (name, try_mode) in tries {
        let verdict = match forced.as_deref() {
            // A forced value is the caller's environment, so the report
            // names the variable and never repeats its value.
            Some(f) if f != name => Err("ONELF_MODE selects another mode".to_string()),
            _ => try_mode(),
        };
        let done = verdict.is_ok() || forced.as_deref() == Some(name);
        out.push((name, verdict));
        if done {
            break;
        }
    }
    out
}

/// Print the report. Nothing is run, nothing is acquired, and nothing it
/// creates outlives it.
pub fn run(pkg: &mut PackageData, ep_idx: usize) {
    print!("{}", report(pkg, ep_idx));
}

/// Assemble the whole report. Every section reads package records or
/// host facts only; the one write anywhere in the report is the scratch
/// extraction the library comparison reads from, which is removed again
/// before the report ends.
pub(crate) fn report(pkg: &mut PackageData, ep_idx: usize) -> String {
    let mut out = String::new();
    let ep_name = pkg
        .manifest
        .get_string(pkg.manifest.entrypoints[ep_idx].name)
        .to_string();
    line(
        &mut out,
        &format!(
            "onelf-rt {}: {} (entrypoint {})",
            env!("CARGO_PKG_VERSION"),
            plain(pkg.manifest.name()),
            plain(&ep_name)
        ),
    );

    line(&mut out, "\nHost");
    let ns = mount::namespace_usable();
    line(
        &mut out,
        &format!(
            "  user namespaces: {}",
            if ns { "usable" } else { "restricted" }
        ),
    );
    match mount::helper() {
        Some(helper) => line(
            &mut out,
            &format!("  mount helper: {}", plain(&helper.display().to_string())),
        ),
        None => line(
            &mut out,
            "  mount helper: none (no fusermount3 or setuid fusermount)",
        ),
    }

    let rules = read_package_file(pkg, pkgmeta::RUNTIME_RULES_FILE);
    let table = read_package_file(pkg, pkgmeta::ENTRYPOINT_SETS_FILE);
    let anchors = read_package_file(pkg, onelf_format::anchors::ANCHORS_FILE);
    // The same verified read the launch applies, so the report never
    // names a pair a launch would refuse whole.
    let paths = match onelf_rt::hostpaths::read(pkg) {
        Ok(pairs) if !pairs.is_empty() => Some(Ok(pairs)),
        Ok(_) => None,
        Err(e) => Some(Err(e.to_string())),
    };
    let needs_setuid = read_package_file(pkg, ".onelf/needs-setuid").is_some();
    // The mode verdicts come first: the pairs' fate follows the mode a
    // launch would pick, and a memfd launch carries no package tree to
    // overlay anything onto.
    let tried = modes(pkg, ep_idx, needs_setuid, ns);
    let chosen = tried.iter().find_map(|(name, verdict)| {
        verdict
            .as_ref()
            .ok()
            .map(|(_, places_host_files)| (*name, *places_host_files))
    });
    // The same refusals a launch's overlay applies, in the same order,
    // each naming the package's pair as that launch would leave it.
    let unredirected = pairs_unredirected(chosen.map(|(name, _)| name), needs_setuid, ns);
    // The carried side is resolved only when a record actually reads
    // it and a launch here would consult it, and resolved once: the
    // manifest can hold tens of thousands of entries against a record
    // bound of 1024 pairs. A launch's overlay mounts the extracted
    // tree's `relative`, and the kernel resolves every symlink on the
    // way, so the lookup walks the manifest the same way: a directory
    // entry descends, a symlink entry follows its target (a bounded
    // number of hops, relative to the link's directory, never above
    // the root), and a file entry or a missing component means the
    // mount would have no directory to bind, and the pair stays
    // unredirected.
    let carried = matches!((&paths, &unredirected), (Some(Ok(_)), None)).then(|| {
        let mut kinds = std::collections::HashMap::new();
        let mut links = std::collections::HashMap::new();
        for (i, entry) in pkg.manifest.entries.iter().enumerate() {
            let path = pkg.manifest.entry_path(i);
            if path.is_empty() {
                continue;
            }
            if entry.kind == onelf_format::EntryKind::Symlink {
                links.insert(
                    path.clone(),
                    pkg.manifest.get_string(entry.symlink_target).to_string(),
                );
            }
            kinds.insert(path, entry.kind);
        }
        (kinds, links)
    });
    let carries_directory = |relative: &str| {
        carried
            .as_ref()
            .is_some_and(|(kinds, links)| carried_directory(kinds, links, relative))
    };
    metadata_section(
        &mut out,
        &rules,
        &table,
        &anchors,
        paths,
        unredirected.as_deref(),
        &carries_directory,
    );

    line(&mut out, "\nExecution mode");
    for (name, verdict) in tried {
        match verdict {
            Ok((how, _)) => line(&mut out, &format!("  {name}: would run, {how}")),
            Err(why) => line(&mut out, &format!("  {name}: skipped, {why}")),
        }
    }
    if chosen.is_none() {
        line(&mut out, "  no mode can run this package here");
    }

    // The entrypoint's selection, resolved the way a launch resolves it,
    // without acquiring any of it.
    let selection = pkgmeta::validate(pkg, &ep_name)
        .map(|meta| meta.pins)
        .map_err(|e| e.to_string());

    let lib_dirs: Vec<String> = pkg
        .manifest
        .lib_dirs()
        .iter()
        .map(|d| d.to_string())
        .collect();
    let sets_weighed = selection.as_ref().is_ok_and(|pins| !pins.is_empty());
    if let Some((name, places_host_files)) = chosen
        && name != "memfd"
        && !lib_dirs.is_empty()
    {
        libraries(
            pkg,
            ep_idx,
            &lib_dirs,
            places_host_files,
            sets_weighed,
            &mut out,
        );
    }
    sets_section(&mut out, &ep_name, &selection, table.is_some());
    view_section(&mut out, pkg, &ep_name, &selection, &rules, &table);
    update(pkg, &mut out);
    out
}

/// The carried metadata records, the runtime support they require, and
/// what this runtime itself advertises. The requirement is the one
/// packaging enforced when the records were emitted; the agreement is
/// re-stated here for the runtime that would actually apply them.
fn metadata_section(
    out: &mut String,
    rules: &Option<Vec<u8>>,
    table: &Option<Vec<u8>>,
    anchors: &Option<Vec<u8>>,
    paths: Option<Result<Vec<PathPair>, String>>,
    unredirected: Option<&str>,
    carries: &dyn Fn(&str) -> bool,
) {
    line(out, "\nMetadata");
    if rules.is_none() && table.is_none() && anchors.is_none() && paths.is_none() {
        line(out, "  records: none carried (legacy defaults)");
        return;
    }
    let decoded = table.as_deref().map(EntrypointSets::decode);
    let mut required = if rules.is_some() || table.is_some() {
        RuntimeRequirements::metadata(elf::RUNTIME_METADATA_VERSION)
    } else {
        RuntimeRequirements::NONE
    };
    if let Some(Ok(record)) = &decoded
        && !record.entrypoints.is_empty()
    {
        required.capabilities = required
            .capabilities
            .union(elf::RuntimeCapabilities::SELECTION_VIEW);
    }
    if let Some(Err(why)) = &decoded {
        line(out, &format!("  selection table: unreadable: {why}"));
    }
    match anchors.as_deref().map(onelf_format::anchors::decode) {
        Some(Ok(record)) if !record.is_empty() => {
            required.capabilities = required
                .capabilities
                .union(elf::RuntimeCapabilities::ANCHORS);
            for anchor in &record {
                line(
                    out,
                    &format!("  anchor {} -> {}", anchor.path(), anchor.relative),
                );
            }
        }
        Some(Err(why)) => line(out, &format!("  anchor record: unreadable: {why}")),
        _ => {}
    }
    let carried_paths = paths.is_some();
    match paths {
        Some(Ok(pairs)) => {
            for pair in &pairs {
                let reason = unredirected
                    .map(|why| why.to_string())
                    .or_else(|| {
                        (!carries(&pair.relative)).then(|| {
                            format!(
                                "the package does not carry {} as a directory",
                                pair.relative
                            )
                        })
                    })
                    .or_else(|| {
                        let target = std::path::Path::new(&pair.absolute);
                        let placeable = target.is_dir()
                            || target.parent().is_some_and(|parent| parent.is_dir());
                        (!placeable)
                            .then(|| format!("no host directory to place {} under", pair.absolute))
                    });
                match reason {
                    Some(why) => line(
                        out,
                        &format!(
                            "  unredirected {} -> {} ({why})",
                            pair.absolute, pair.relative
                        ),
                    ),
                    None => line(
                        out,
                        &format!("  overlay {} -> {}", pair.absolute, pair.relative),
                    ),
                }
            }
        }
        Some(Err(why)) => line(out, &format!("  paths record: unreadable: {why}")),
        None => {}
    }
    let mut carried: Vec<&str> = Vec::new();
    if rules.is_some() {
        carried.push("runtime-rules");
    }
    if table.is_some() {
        carried.push("entrypoint-sets");
    }
    if anchors.is_some() {
        carried.push("anchors");
    }
    if carried_paths {
        carried.push("paths");
    }
    line(out, &format!("  records: {}", carried.join(", ")));
    line(
        out,
        &format!("  requires: {}", requirements_text(&required)),
    );
    let advertised = crate::capability::advertised();
    line(
        out,
        &format!(
            "  this runtime advertises: {}",
            advertisement_text(&advertised)
        ),
    );
    match elf::check_runtime_support(&advertised, required) {
        Ok(()) => line(out, "  agreement: satisfied"),
        Err(why) => line(out, &format!("  agreement: unsatisfied: {why}")),
    }
}

/// The support `required` names, as one report line: the metadata
/// schema version, then any named capability.
fn requirements_text(required: &RuntimeRequirements) -> String {
    let mut parts = Vec::new();
    if required.metadata_version != 0 {
        parts.push(format!(
            "payload metadata version {}",
            required.metadata_version
        ));
    }
    if !required.capabilities.is_empty() {
        parts.push(required.capabilities.to_string());
    }
    if parts.is_empty() {
        return "(none)".to_string();
    }
    parts.join(", ")
}

/// What a note advertises, as one report line.
fn advertisement_text(note: &RuntimeNote) -> String {
    match note {
        RuntimeNote::Present {
            metadata_version,
            capabilities,
        } => format!("payload metadata version {metadata_version}, {capabilities}"),
        RuntimeNote::Absent => {
            "no capability note (a runtime from before payload metadata)".to_string()
        }
        RuntimeNote::Incompatible(why) => format!("an unreadable capability note: {why}"),
    }
}

fn libraries(
    pkg: &mut PackageData,
    ep_idx: usize,
    lib_dirs: &[String],
    places_host_files: bool,
    sets_weighed: bool,
    out: &mut String,
) {
    line(out, "\nLibraries");
    let disabled = env_set("ONELF_NO_RESOLVER");
    let policy = if disabled {
        HostLibsPolicy::Never
    } else {
        HostLibsPolicy::from_flags(pkg.footer.flags)
    };
    line(
        out,
        &format!(
            "  host-libs: {}{}",
            policy.as_str(),
            if disabled { " (ONELF_NO_RESOLVER)" } else { "" }
        ),
    );

    let Some(scratch) = onelf_rt::paths::private_dir()
        .map(|d| d.join(format!("onelf-doctor-{}", std::process::id())))
    else {
        line(
            out,
            "  no private runtime directory to read the libraries in",
        );
        return;
    };
    let ep = &pkg.manifest.entrypoints[ep_idx];
    let executable = std::path::PathBuf::from(pkg.manifest.entry_path(ep.target_entry as usize));
    let wanted = |rel: &Path| {
        rel == executable
            || rel
                .parent()
                .is_some_and(|p| lib_dirs.iter().any(|d| p == Path::new(d)))
    };
    let extracted = std::fs::create_dir(&scratch)
        .and_then(|()| onelf_rt::cache::extract_matching(pkg, &scratch, wanted));
    match extracted {
        Ok(()) => verdicts(
            pkg,
            &scratch,
            &scratch.join(&executable),
            lib_dirs,
            policy,
            places_host_files,
            sets_weighed,
            out,
        ),
        Err(e) => line(
            out,
            &format!(
                "  cannot read the bundled libraries: {}",
                plain(&e.to_string())
            ),
        ),
    }
    let _ = std::fs::remove_dir_all(&scratch);
}

// Describes one resolver comparison; every argument is distinct and a
// parameter object would be built at each call site and read once.
#[allow(clippy::too_many_arguments)]
fn verdicts(
    pkg: &mut PackageData,
    root: &Path,
    executable: &Path,
    lib_dirs: &[String],
    policy: HostLibsPolicy,
    places_host_files: bool,
    sets_weighed: bool,
    out: &mut String,
) {
    let lib_dirs: Vec<&str> = lib_dirs.iter().map(String::as_str).collect();
    let ld_cache = drivers::cache_file();
    let gl = resolve::gl_situation(
        root,
        &lib_dirs,
        &[],
        Some(executable),
        &ld_cache,
        resolve::ICD_DIRS,
    );
    let gl_text = match gl {
        Gl::Bundled => "bundled",
        Gl::Host => "from the host",
        Gl::Missing => "none on the host, which the bundle links",
        Gl::Absent => "none on the host or in the bundle; nothing links one",
    };
    line(out, &format!("  GL stack: {gl_text}"));
    let pin = read_package_file(pkg, onelf_rt::platform::PIN_FILE)
        .and_then(|b| onelf_rt::platform::parse_pin(&String::from_utf8_lossy(&b)));
    if let Some(pin) = &pin {
        let wanted = if gl.lacking() {
            "the launch uses it"
        } else {
            "not needed here"
        };
        line(
            out,
            &format!(
                "  pinned GL build: {}, {}; {wanted}",
                pin.label,
                stored(pin)
            ),
        );
    }

    if policy == HostLibsPolicy::Never {
        line(
            out,
            "  host modules: none considered; the policy admits no module from the host",
        );
        return;
    }
    let Some(store) = onelf_rt::paths::resolve_store(&pkg.manifest.header.package_id) else {
        line(
            out,
            "  no private runtime directory for the resolver's store",
        );
        return;
    };
    if !places_host_files {
        line(
            out,
            "  this mode cannot put a host file over a bundled one, so glibc stays bundled",
        );
    }
    if gl.lacking() && pin.is_some() {
        line(out, "  the pinned build's libraries are not weighed below");
    }
    if sets_weighed {
        line(
            out,
            "  the dependency sets' libraries are not weighed below",
        );
    }
    let loader_place = onelf_rt::cache::LoaderPlace::for_this_uid();
    let plugin_dirs = match onelf_rt::hostpaths::read_plugin_dirs(pkg) {
        Ok(dirs) => dirs,
        Err(e) => {
            line(
                out,
                &format!(
                    "  plugin directories: the record is refused, so no host module \
                     is considered ({e})"
                ),
            );
            Vec::new()
        }
    };
    let decision = resolve::decide(&Request {
        pkg_root: root,
        lib_dirs: &lib_dirs,
        plugin_dirs: &plugin_dirs,
        policy,
        store: &store,
        ld_cache: &ld_cache,
        icd_dirs: resolve::ICD_DIRS,
        extra_root: None,
        set_roots: &[],
        host_libc: places_host_files,
        loader_store: loader_place.store(),
        nsswitch: resolve::nsswitch_file(),
        uid: rustix::process::getuid().as_raw(),
        trust: None,
    });
    let mut host_only: Vec<String> = Vec::new();
    for (name, choice) in &decision.compared {
        let host = decision
            .winners
            .get(name)
            .map(|p| plain(&p.display().to_string()));
        let name = plain(name);
        match (choice, host) {
            (None, _) => host_only.push(name),
            (Some(Choice::Host), Some(host)) => {
                line(out, &format!("  {name}: the host's is newer, {host}"))
            }
            (Some(Choice::Host), None) => line(out, &format!("  {name}: the host's is newer")),
            (Some(Choice::Bundle), _) => line(out, &format!("  {name}: bundled copy")),
            (Some(Choice::Incomparable), _) => {
                line(out, &format!("  {name}: incomparable, bundled copy kept"))
            }
        }
    }
    if !host_only.is_empty() {
        line(
            out,
            &format!(
                "  from the host, not bundled ({}): {}",
                host_only.len(),
                host_only.join(", ")
            ),
        );
    }
    if decision.compared.is_empty() {
        line(
            out,
            "  nothing to compare: the host offers no driver libraries",
        );
    }
    if let Some(loader) = &decision.host_interp {
        line(
            out,
            &format!(
                "  loader: the host's {}, run from a private copy closed to its default \
                 search directories",
                plain(&loader.source.display().to_string())
            ),
        );
    }
    if let Some((source, why)) = &decision.host_interp_refused {
        line(
            out,
            &format!(
                "  loader: the host's {} is not used, {}; the bundled loader and libc run",
                plain(&source.display().to_string()),
                plain(why)
            ),
        );
    }
    if let Some(farm) = &decision.farm {
        // The farm lives under the private runtime directory, whose place
        // the environment chose, so the report states the fact alone.
        let state = if farm.is_dir() {
            "present in the resolver's store"
        } else {
            "none yet, a launch creates it"
        };
        line(out, &format!("  link farm: {state}"));
    }
    if !decision.admitted.is_empty() {
        line(
            out,
            &format!("  host modules admitted ({}):", decision.admitted.len()),
        );
        for module in &decision.admitted {
            line(
                out,
                &format!("    {}", plain(&module.display().to_string())),
            );
        }
    }
    if !decision.refused.is_empty() {
        line(
            out,
            &format!("  host modules refused ({}):", decision.refused.len()),
        );
        for refusal in &decision.refused {
            line(
                out,
                &format!(
                    "    {}: {} {}",
                    plain(&refusal.module.display().to_string()),
                    plain(&refusal.object.display().to_string()),
                    plain(&refusal.reason)
                ),
            );
        }
    }
}

/// Whether the store holds the build `pin` names, or where it comes
/// from. The store's place follows the environment, so the report states
/// the fact, never the path.
fn stored(pin: &Pin) -> String {
    match onelf_rt::platform::stored_path(pin) {
        Ok(file) if file.is_file() => "stored in the platform store".to_string(),
        _ if env_set("ONELF_NO_PLATFORM_FETCH") => {
            "not stored, and ONELF_NO_PLATFORM_FETCH forbids fetching it".to_string()
        }
        _ => format!("not stored, a launch fetches {}", pin.url),
    }
}

/// The entrypoint's selected sets and, for each, whether the store
/// already holds its build or a launch would fetch it. Only selected
/// pins are named and only their store entries are consulted: an
/// unselected set is not read, stat'd, or warned about, and an empty
/// selection names none at all. Nothing is fetched here.
fn sets_section(
    out: &mut String,
    ep_name: &str,
    selection: &Result<Vec<Pin>, String>,
    carries_record: bool,
) {
    let pins = match selection {
        Ok(pins) => pins,
        Err(why) => {
            line(
                out,
                &format!(
                    "\nDependency sets\n  cannot resolve the selection for {}: {why}",
                    plain(ep_name)
                ),
            );
            return;
        }
    };
    if pins.is_empty() && !carries_record {
        return;
    }
    line(out, "\nDependency sets");
    if pins.is_empty() {
        line(
            out,
            &format!(
                "  selected for {}: none; no set is fetched, mounted, or extracted",
                plain(ep_name)
            ),
        );
        return;
    }
    let scope = if carries_record {
        pins.iter()
            .map(|pin| pin.label.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        "every declared set (the package carries no selection record)".to_string()
    };
    line(out, &format!("  selected for {}: {scope}", plain(ep_name)));
    for pin in pins {
        line(out, &format!("  {}: {}", pin.label, stored(pin)));
    }
}

/// The execution view a launch of this entrypoint would publish, stated
/// as the identity components this report can know without acquiring
/// anything: the package, the entrypoint, its selected pins, the carried
/// metadata's digest, and the consumer. A launch completes the identity
/// with its host-libs decision, resolver links, and loader choice, and
/// only then publishes the view. This report publishes nothing and
/// mutates no view.
fn view_section(
    out: &mut String,
    pkg: &PackageData,
    ep_name: &str,
    selection: &Result<Vec<Pin>, String>,
    rules: &Option<Vec<u8>>,
    table: &Option<Vec<u8>>,
) {
    line(out, "\nExecution view");
    line(
        out,
        &format!("  package: {}", cache::hex(&pkg.manifest.header.package_id)),
    );
    line(out, &format!("  entrypoint: {}", plain(ep_name)));
    match selection {
        Ok(pins) if pins.is_empty() => line(out, "  selected pins: none"),
        Ok(pins) => {
            line(out, "  selected pins:");
            for pin in pins {
                line(out, &format!("    {} {}", pin.label, pin.blake3));
            }
        }
        Err(_) => line(out, "  selected pins: the selection cannot be resolved"),
    }
    match crate::metadata_digest(rules.clone(), table.clone()) {
        None => line(out, "  metadata: built-in defaults"),
        Some(digest) => line(out, &format!("  metadata: {}", cache::hex(&digest))),
    }
    line(out, &format!("  consumer: uid {}", onelf_rt::paths::uid()));
    line(
        out,
        "  completed at launch by: the host-libs policy, the resolver's links, and the loader choice",
    );
    line(out, "  this report publishes nothing");
}

fn update(pkg: &mut PackageData, out: &mut String) {
    line(out, "\nSelf-update");
    let text = |pkg: &mut PackageData, path: &str| {
        read_package_file(pkg, path).map(|b| String::from_utf8_lossy(&b).trim().to_string())
    };
    let Some(url) = text(pkg, ".onelf/update-url") else {
        line(out, "  not configured");
        return;
    };
    line(out, &format!("  url: {}", plain(&url)));
    let unsigned = read_package_file(pkg, ".onelf/update-unsigned").is_some();
    match read_package_file(pkg, ".onelf/update-key") {
        Some(keys) => line(out, &format!("  signing keys: {}", keys.len() / 32)),
        None if unsigned => {
            line(
                out,
                "  signing keys: none (updates are not signature-verified)",
            );
        }
        None => line(out, "  signing keys: none, so --onelf-update refuses"),
    }
    if let Some(revision) = text(pkg, ".onelf/update-revision") {
        line(out, &format!("  revision: {}", plain(&revision)));
    }
    if cfg!(feature = "update") {
        line(out, "  updater: in this runtime");
    } else {
        line(
            out,
            "  updater: not in this runtime, so the update flags reach the app",
        );
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_empty_requirement_reads_none() {
        let none = super::RuntimeRequirements::NONE;
        assert_eq!(super::requirements_text(&none), "(none)");
    }

    use std::fs::File;
    use std::path::PathBuf;

    use onelf_format::rules::RuleId;
    use onelf_format::runtime_metadata::{EntrypointSelection, EntrypointSets, SetPin};
    use onelf_format::{
        Block, Entry, EntryKind, EntryPoint, EntryPointFlags, Flags, Footer, Manifest,
        ManifestHeader, WorkingDir,
    };

    use super::{metadata_section, pairs_unredirected, report, sets_section, view_section};
    use crate::loader;
    use onelf_format::paths::PathPair;

    const PIN_QT: [u8; 32] = [0xaa; 32];
    const PIN_GTK: [u8; 32] = [0xbb; 32];

    /// A fixture package loaded through the real loader, with its file
    /// removed when the case goes away.
    struct Case {
        pkg: loader::PackageData,
        path: PathBuf,
    }

    impl Drop for Case {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn case(name: &str, files: &[(&str, Vec<u8>)], entrypoints: &[(&str, &str)]) -> Case {
        let path = std::env::temp_dir().join(format!("onelf-doctor-{name}-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let built = fixture::build(b"onelf fixture prefix", files, entrypoints);
        std::fs::write(&path, &built.bytes).unwrap();
        let pkg = loader::load_file(File::open(&path).unwrap()).unwrap();
        Case { pkg, path }
    }

    fn sets_table_text() -> Vec<u8> {
        b"[qt]\nurl = \"https://e/qt.onelf\"\nblake3 = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n\
          [gtk]\nurl = \"https://e/gtk.onelf\"\nblake3 = \"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\"\n"
            .to_vec()
    }

    /// The selection record `selections` describes: one entry per
    /// `(entrypoint, sets)` pair over the qt and gtk pins.
    fn record(selections: &[(&str, &[&str])]) -> Vec<u8> {
        EntrypointSets {
            pins: vec![
                SetPin {
                    name: RuleId::new("qt").unwrap(),
                    hash: PIN_QT,
                },
                SetPin {
                    name: RuleId::new("gtk").unwrap(),
                    hash: PIN_GTK,
                },
            ],
            entrypoints: selections
                .iter()
                .map(|(name, sets)| EntrypointSelection {
                    name: (*name).to_string(),
                    sets: sets.iter().map(|s| RuleId::new(s).unwrap()).collect(),
                })
                .collect(),
        }
        .encode()
        .unwrap()
    }

    fn pin(name: &str, hash: [u8; 32]) -> onelf_rt::platform::Pin {
        onelf_rt::platform::Pin {
            label: name.to_string(),
            url: format!("https://e/{name}.onelf"),
            blake3: crate::cache::hex(&hash),
        }
    }

    /// The `Dependency sets` lines for one entrypoint's selection.
    fn sets_lines(ep: &str, pins: &[onelf_rt::platform::Pin], carries_record: bool) -> String {
        let mut out = String::new();
        sets_section(&mut out, ep, &Ok(pins.to_vec()), carries_record);
        out
    }

    /// An empty selection names none of the declared sets: the
    /// unselected pins are not read, stat'd, or warned about, so their
    /// absence or corruption cannot change the report.
    #[test]
    fn an_empty_selection_names_no_unselected_pin() {
        let lines = sets_lines("cli", &[], true);
        assert!(lines.contains("selected for cli: none"), "{lines}");
        assert!(
            lines.contains("no set is fetched, mounted, or extracted"),
            "{lines}"
        );
        assert!(!lines.contains("qt"), "{lines}");
        assert!(!lines.contains("gtk"), "{lines}");
    }

    /// A selection names exactly its own pins and nothing else.
    #[test]
    fn a_selection_names_exactly_its_own_pins() {
        let lines = sets_lines("gui", &[pin("qt", PIN_QT)], true);
        assert!(lines.contains("selected for gui: qt"), "{lines}");
        assert!(lines.contains("qt:"), "{lines}");
        assert!(!lines.contains("gtk"), "{lines}");
        let both = sets_lines("all", &[pin("gtk", PIN_GTK), pin("qt", PIN_QT)], true);
        assert!(
            both.contains("selected for all: gtk, qt")
                && both.contains("  gtk:")
                && both.contains("  qt:"),
            "{both}"
        );
    }

    /// A package without a selection record keeps the legacy reading:
    /// every declared set, and the report says so.
    #[test]
    fn the_legacy_reading_names_every_declared_set() {
        let lines = sets_lines("app", &[pin("qt", PIN_QT), pin("gtk", PIN_GTK)], false);
        assert!(
            lines.contains(
                "selected for app: every declared set (the package carries no selection record)"
            ),
            "{lines}"
        );
        // A package that declares no sets gets no section at all.
        let bare = sets_lines("app", &[], false);
        assert!(bare.is_empty(), "{bare}");
    }

    /// The view section states the identity components known without
    /// acquiring anything, and says that nothing is published.
    #[test]
    fn the_view_section_states_what_is_known_without_acquisition() {
        let case = case(
            "view",
            &[("bin/app", b"elf bytes".to_vec())],
            &[("app", "0")],
        );
        let none = view_lines(&case.pkg, "app", &Ok(Vec::new()), &None, &None);
        assert!(none.contains("  selected pins: none"), "{none}");
        assert!(none.contains("  metadata: built-in defaults"), "{none}");
        assert!(none.contains("publishes nothing"), "{none}");
        assert!(
            none.contains(&format!("  package: {}", crate::cache::hex(&[0u8; 32]))),
            "{none}"
        );
        assert!(none.contains("  consumer: uid "), "{none}");

        let qt = vec![pin("qt", PIN_QT)];
        let with = view_lines(
            &case.pkg,
            "app",
            &Ok(qt),
            &Some(b"rules".to_vec()),
            &Some(b"table".to_vec()),
        );
        assert!(with.contains("    qt aaaa"), "{with}");
        assert!(
            !with.contains("  metadata: built-in defaults") && with.contains("  metadata: "),
            "{with}"
        );
    }

    fn view_lines(
        pkg: &loader::PackageData,
        ep: &str,
        selection: &Result<Vec<onelf_rt::platform::Pin>, String>,
        rules: &Option<Vec<u8>>,
        table: &Option<Vec<u8>>,
    ) -> String {
        let mut out = String::new();
        view_section(&mut out, pkg, ep, selection, rules, table);
        out
    }

    /// The full report of a record-carrying package states the records,
    /// the support they require, and the agreement, and the empty
    /// selection keeps the unselected sets out of the report entirely.
    #[test]
    fn the_report_explains_metadata_selections_and_the_view() {
        let mut case = case(
            "full",
            &[
                ("bin/app", b"elf bytes".to_vec()),
                ("bin/cli", b"elf bytes".to_vec()),
                (crate::sets::SETS_FILE, sets_table_text()),
                (
                    crate::pkgmeta::ENTRYPOINT_SETS_FILE,
                    record(&[("app", &[]), ("gui", &["qt"])]),
                ),
            ],
            &[("app", "0"), ("gui", "1")],
        );

        let empty = report(&mut case.pkg, 0);
        assert!(empty.contains("\nMetadata\n"), "{empty}");
        assert!(empty.contains("records: entrypoint-sets"), "{empty}");
        assert!(
            empty.contains("requires: payload metadata version 1, selection-view"),
            "{empty}"
        );
        assert!(
            empty.contains("this runtime advertises: payload metadata version 1, selection-view"),
            "{empty}"
        );
        assert!(empty.contains("agreement: satisfied"), "{empty}");
        assert!(
            empty.contains("selected for app: none; no set is fetched, mounted, or extracted"),
            "{empty}"
        );
        assert!(!empty.contains("qt"), "{empty}");
        assert!(!empty.contains("gtk"), "{empty}");
        assert!(empty.contains("this report publishes nothing"), "{empty}");

        let selected = report(&mut case.pkg, 1);
        assert!(selected.contains("selected for gui: qt"), "{selected}");
        assert!(selected.contains("qt:"), "{selected}");
        assert!(!selected.contains("gtk"), "{selected}");
    }

    /// The pairs record is named pair by pair: overlaid when a launch
    /// here would bind it, unredirected with the reason when it would
    /// not, and an unreadable record says so instead of naming pairs.
    #[test]
    fn a_paths_record_is_named_overlay_or_unredirected() {
        // The absolute side sits under the temporary directory, whose
        // parent exists, so the host-side place check passes and the
        // verdict is the overlay itself.
        let base = std::env::temp_dir();
        let place = |name: &str| base.join(name).to_string_lossy().into_owned();
        let pairs = vec![
            PathPair {
                absolute: place("e-fw"),
                relative: "share/fw".to_string(),
            },
            PathPair {
                absolute: place("m-vendor"),
                relative: "share/vendor".to_string(),
            },
        ];
        let overlay =
            |absolute: &str, relative: &str| format!("  overlay {absolute} -> {relative}");
        let mut out = String::new();
        metadata_section(
            &mut out,
            &None,
            &None,
            &None,
            Some(Ok(pairs.clone())),
            None,
            &|_| true,
        );
        assert!(
            out.contains(&overlay(&pairs[0].absolute, "share/fw")),
            "{out}"
        );
        assert!(
            out.contains(&overlay(&pairs[1].absolute, "share/vendor")),
            "{out}"
        );
        assert!(out.contains("records: paths"), "{out}");
        assert!(!out.contains("  anchor "), "{out}");

        let mut out = String::new();
        metadata_section(
            &mut out,
            &None,
            &None,
            &None,
            Some(Ok(pairs)),
            Some("ONELF_FUSE_NO_NAMESPACE is set"),
            &|_| true,
        );
        assert!(
            out.contains(&format!(
                "  unredirected {} -> share/fw (ONELF_FUSE_NO_NAMESPACE is set)",
                place("e-fw")
            )),
            "{out}"
        );
        assert!(!out.contains("  overlay "), "{out}");

        // A pair whose bundle side the package does not carry as a
        // directory is not overlaid by the launch, and the report says
        // so instead of promising one.
        let mut out = String::new();
        metadata_section(
            &mut out,
            &None,
            &None,
            &None,
            Some(Ok(vec![PathPair {
                absolute: "/e/fw".to_string(),
                relative: "share/gone".to_string(),
            }])),
            None,
            &|relative| relative == "share/here",
        );
        assert!(
            out.contains(
                "  unredirected /e/fw -> share/gone (the package does not carry share/gone as a directory)"
            ),
            "{out}"
        );

        // A pair whose absolute side the host has no place for is not
        // overlaid either: the launch would skip it.
        let mut out = String::new();
        metadata_section(
            &mut out,
            &None,
            &None,
            &None,
            Some(Ok(vec![PathPair {
                absolute: "/nonexistent-parent/fw".to_string(),
                relative: "share/here".to_string(),
            }])),
            None,
            &|_| true,
        );
        assert!(
            out.contains(
                "  unredirected /nonexistent-parent/fw -> share/here (no host directory to place /nonexistent-parent/fw under)"
            ),
            "{out}"
        );

        let mut out = String::new();
        metadata_section(
            &mut out,
            &None,
            &None,
            &None,
            Some(Err(".onelf/paths: bad".to_string())),
            None,
            &|_| true,
        );
        assert!(out.contains("paths record: unreadable"), "{out}");
        assert!(!out.contains("overlay"), "{out}");
    }

    /// The pairs' fate follows the launch: no mode, a memfd launch's
    /// missing tree, the package's setuid request, or a restricted host
    /// each names the pairs unredirected; a runnable mode names none.
    #[test]
    fn the_pair_fate_follows_the_launch() {
        assert_eq!(
            pairs_unredirected(None, false, true).as_deref(),
            Some("no execution mode can run this package here")
        );
        assert_eq!(
            pairs_unredirected(Some("memfd"), false, true).as_deref(),
            Some("the entrypoint runs from memory, which carries no package tree")
        );
        assert_eq!(
            pairs_unredirected(Some("rundir"), true, true).as_deref(),
            Some(super::SETUID_REFUSAL)
        );
        assert_eq!(
            pairs_unredirected(Some("rundir"), false, false).as_deref(),
            Some(super::NS_RESTRICTED)
        );
        assert_eq!(pairs_unredirected(Some("rundir"), false, true), None);
    }

    /// A pair whose bundle side the package carries as a symlink is
    /// overlaid too: the launch's `is_dir` resolves the tree's links.
    #[test]
    fn a_pair_through_a_symlink_is_named_overlay() {
        let restricted = !onelf_rt::fuse::mount::namespace_usable();
        if restricted {
            eprintln!("this host restricts namespaces: the pair is named unredirected");
        }
        let absolute = std::env::temp_dir().join("onelf-doctor-symlink-place");
        let record = format!("{}\tshare/fw", absolute.display());
        let built = fixture::build_flagged(
            b"onelf fixture prefix",
            &[
                ("bin/app", b"elf bytes".to_vec()),
                ("share/real/x", b"real".to_vec()),
                (onelf_format::paths::PATHS_FILE, record.into_bytes()),
            ],
            &[("share/fw", "real")],
            &[("app", "0")],
            EntryPointFlags::empty(),
        );
        let path =
            std::env::temp_dir().join(format!("onelf-doctor-symlink-{}", std::process::id()));
        std::fs::write(&path, &built.bytes).unwrap();
        let mut pkg = loader::load_file(File::open(&path).unwrap()).unwrap();
        drop(std::fs::remove_file(&path));
        let out = report(&mut pkg, 0);
        if restricted {
            // Some mode may still run; the namespace is what the pair
            // verdict names here.
            assert!(
                out.contains(&format!(
                    "  unredirected {} -> share/fw",
                    absolute.display()
                )),
                "{out}"
            );
            return;
        }
        assert!(
            out.contains(&format!("  overlay {} -> share/fw", absolute.display())),
            "{out}"
        );
        assert!(!out.contains("  unredirected "), "{out}");
    }

    /// The carried-side lookup resolves like the kernel does: a pair
    /// descending through a symlinked directory is still carried, a
    /// symlink to a file carries no directory, a crafted chain or
    /// cycle ends the lookup, a resolution above the root escapes it,
    /// and a missing component carries nothing.
    #[test]
    fn the_carried_lookup_resolves_symlinks_like_the_kernel() {
        use onelf_format::EntryKind;
        let mut kinds = std::collections::HashMap::from([
            ("share".to_string(), EntryKind::Dir),
            ("share/fw".to_string(), EntryKind::Symlink),
            ("share/real".to_string(), EntryKind::Dir),
            ("share/real/lib".to_string(), EntryKind::Dir),
            ("share/data".to_string(), EntryKind::File),
            ("share/top".to_string(), EntryKind::Symlink),
        ]);
        let mut links = std::collections::HashMap::from([
            ("share/fw".to_string(), "real".to_string()),
            ("share/top".to_string(), "fw".to_string()),
        ]);
        // Through one symlink, and through a chain of two.
        assert!(super::carried_directory(&kinds, &links, "share/fw"));
        assert!(super::carried_directory(&kinds, &links, "share/fw/lib"));
        assert!(super::carried_directory(&kinds, &links, "share/top/lib"));
        // A symlink to a file, and a plain missing component, carry no
        // directory.
        kinds.insert("share/link".to_string(), EntryKind::Symlink);
        links.insert("share/link".to_string(), "data".to_string());
        assert!(!super::carried_directory(&kinds, &links, "share/link"));
        assert!(!super::carried_directory(&kinds, &links, "share/gone/lib"));
        // A multi-component target walks through an intermediate link,
        // and a link resolving to the root resolves like the kernel,
        // with the rest of the path walking on from there.
        kinds.insert("share/into".to_string(), EntryKind::Symlink);
        links.insert("share/into".to_string(), "alias/lib".to_string());
        kinds.insert("share/alias".to_string(), EntryKind::Symlink);
        links.insert("share/alias".to_string(), "real".to_string());
        assert!(super::carried_directory(&kinds, &links, "share/into"));
        kinds.insert("share/up".to_string(), EntryKind::Symlink);
        links.insert("share/up".to_string(), "..".to_string());
        assert!(super::carried_directory(&kinds, &links, "share/up"));
        assert!(super::carried_directory(
            &kinds,
            &links,
            "share/up/share/real"
        ));
        // A cycle, a chain longer than the bound, and an escape above
        // the root each end the lookup.
        kinds.insert("share/loop".to_string(), EntryKind::Symlink);
        links.insert("share/loop".to_string(), "loop".to_string());
        assert!(!super::carried_directory(&kinds, &links, "share/loop"));
        for i in 1..=9 {
            let name = format!("share/c{i}");
            kinds.insert(name.clone(), EntryKind::Symlink);
            let target = if i == 9 {
                "real".to_string()
            } else {
                format!("c{}", i + 1)
            };
            links.insert(name, target);
        }
        assert!(!super::carried_directory(&kinds, &links, "share/c1"));
        links.insert("share/escape".to_string(), "../../etc".to_string());
        kinds.insert("share/escape".to_string(), EntryKind::Symlink);
        assert!(!super::carried_directory(&kinds, &links, "share/escape"));
    }

    /// A pair whose bundle side the package carries as a symlink to a
    /// file binds no directory: the report names it unredirected with
    /// the not-carried reason instead of promising an overlay.
    #[test]
    fn a_pair_over_a_symlink_to_a_file_is_not_named_overlay() {
        let restricted = !onelf_rt::fuse::mount::namespace_usable();
        let absolute = std::env::temp_dir().join("onelf-doctor-linkfile-place");
        let record = format!("{}\tshare/fw", absolute.display());
        let built = fixture::build_flagged(
            b"onelf fixture prefix",
            &[
                ("bin/app", b"elf bytes".to_vec()),
                ("share/data", b"not a directory".to_vec()),
                (onelf_format::paths::PATHS_FILE, record.into_bytes()),
            ],
            &[("share/fw", "data")],
            &[("app", "0")],
            EntryPointFlags::empty(),
        );
        let path =
            std::env::temp_dir().join(format!("onelf-doctor-linkfile-{}", std::process::id()));
        std::fs::write(&path, &built.bytes).unwrap();
        let mut pkg = loader::load_file(File::open(&path).unwrap()).unwrap();
        drop(std::fs::remove_file(&path));
        let out = report(&mut pkg, 0);
        if restricted {
            assert!(
                out.contains(&format!(
                    "  unredirected {} -> share/fw",
                    absolute.display()
                )),
                "{out}"
            );
            return;
        }
        assert!(
            out.contains(&format!(
                "  unredirected {} -> share/fw (the package does not carry share/fw as a \
                 directory)",
                absolute.display()
            )),
            "{out}"
        );
        assert!(!out.contains("  overlay "), "{out}");
    }

    /// A memfd launch carries no package tree, so its pairs are named
    /// unredirected with that reason, whatever the host's namespaces:
    /// the mode verdicts decide the pairs' fate before the report.
    #[test]
    fn a_memfd_launch_names_its_pairs_unredirected() {
        let built = fixture::build_flagged(
            b"onelf fixture prefix",
            &[
                ("bin/app", b"elf bytes".to_vec()),
                (onelf_format::paths::PATHS_FILE, b"/e/fw\tshare/fw".to_vec()),
            ],
            &[],
            &[("app", "0")],
            EntryPointFlags::MEMFD_ELIGIBLE,
        );
        let path = std::env::temp_dir().join(format!("onelf-doctor-memfd-{}", std::process::id()));
        std::fs::write(&path, &built.bytes).unwrap();
        let mut pkg = loader::load_file(File::open(&path).unwrap()).unwrap();
        drop(std::fs::remove_file(&path));
        let out = report(&mut pkg, 0);
        assert!(
            out.contains(
                "  unredirected /e/fw -> share/fw (the entrypoint runs from memory, \
                 which carries no package tree)"
            ),
            "{out}"
        );
        assert!(!out.contains("  overlay "), "{out}");
    }

    /// A package without records keeps the legacy defaults and says so.
    #[test]
    fn a_package_without_records_reports_the_legacy_defaults() {
        let mut case = case(
            "legacy",
            &[("bin/app", b"elf bytes".to_vec())],
            &[("app", "0")],
        );
        let empty = report(&mut case.pkg, 0);
        assert!(
            empty.contains("records: none carried (legacy defaults)"),
            "{empty}"
        );
        assert!(!empty.contains("Dependency sets"), "{empty}");
        assert!(
            empty.contains("Execution view") && empty.contains("selected pins: none"),
            "{empty}"
        );
        assert!(empty.contains("metadata: built-in defaults"), "{empty}");
    }

    /// Minimal stored-mode packages the runtime loader can open.
    ///
    /// The layout is `[prefix][manifest][stored payload][footer]`: one
    /// block per file, bytes verbatim.
    mod fixture {
        use super::*;

        pub(crate) struct Built {
            pub(crate) bytes: Vec<u8>,
        }

        /// Build a package carrying `files`, whose entrypoints are
        /// `(name, target entry index)` pairs.
        pub(crate) fn build(
            prefix: &[u8],
            files: &[(&str, Vec<u8>)],
            entrypoints: &[(&str, &str)],
        ) -> Built {
            build_flagged(prefix, files, &[], entrypoints, EntryPointFlags::empty())
        }

        /// [`build`] with every entrypoint carrying `flags` and the
        /// package carrying `symlinks` as `(path, target)` entries.
        /// Entries are hierarchical, one entry per path component with
        /// parent indexes, the shape the packer itself emits, so the
        /// doctor's carried-side walk resolves against a real tree.
        pub(crate) fn build_flagged(
            prefix: &[u8],
            files: &[(&str, Vec<u8>)],
            symlinks: &[(&str, &str)],
            entrypoints: &[(&str, &str)],
            flags: EntryPointFlags,
        ) -> Built {
            let mut string_table: Vec<u8> = vec![0];
            let mut names: std::collections::HashMap<String, u32> =
                std::collections::HashMap::new();
            let mut intern = |name: &str| -> u32 {
                if let Some(&offset) = names.get(name) {
                    return offset;
                }
                let offset = string_table.len() as u32;
                string_table.extend_from_slice(name.as_bytes());
                string_table.push(0);
                names.insert(name.to_string(), offset);
                offset
            };

            // The directory entries everything hangs from, parents
            // before children, each once.
            let mut dir_paths: Vec<String> = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for path in files
                .iter()
                .map(|(p, _)| *p)
                .chain(symlinks.iter().map(|(p, _)| *p))
            {
                let mut chain: Vec<String> = Vec::new();
                let mut parent = path.rsplit_once('/').map(|(p, _)| p.to_string());
                while let Some(p) = parent {
                    if !seen.insert(p.clone()) {
                        break;
                    }
                    parent = p.rsplit_once('/').map(|(pp, _)| pp.to_string());
                    chain.push(p);
                }
                chain.reverse();
                dir_paths.extend(chain);
            }

            let mut payload = Vec::new();
            let mut entries: Vec<Entry> = Vec::new();
            let mut dir_index: std::collections::HashMap<String, u32> =
                std::collections::HashMap::new();
            for path in &dir_paths {
                let (parent_path, name) = match path.rsplit_once('/') {
                    Some((p, n)) => (Some(p.to_string()), n),
                    None => (None, path.as_str()),
                };
                entries.push(Entry {
                    kind: EntryKind::Dir,
                    parent: parent_path.map(|p| dir_index[&p]).unwrap_or(u32::MAX),
                    name: intern(name),
                    mode: 0o755,
                    mtime_secs: 0,
                    mtime_nsec: 0,
                    content_hash: [0; 32],
                    blocks: Vec::new(),
                    symlink_target: 0,
                });
                dir_index.insert(path.clone(), (entries.len() - 1) as u32);
            }
            let mut file_indices: Vec<u32> = Vec::new();
            for (path, content) in files {
                let offset = payload.len() as u64;
                payload.extend_from_slice(content);
                let hash = *blake3::hash(content).as_bytes();
                let (parent_path, name) = match path.rsplit_once('/') {
                    Some((p, n)) => (Some(p.to_string()), n),
                    None => (None, *path),
                };
                entries.push(Entry {
                    kind: EntryKind::File,
                    parent: parent_path.map(|p| dir_index[&p]).unwrap_or(u32::MAX),
                    name: intern(name),
                    mode: 0o755,
                    mtime_secs: 0,
                    mtime_nsec: 0,
                    content_hash: hash,
                    blocks: vec![Block {
                        payload_offset: offset,
                        compressed_size: content.len() as u64,
                        original_size: content.len() as u64,
                        content_hash: hash,
                    }],
                    symlink_target: 0,
                });
                file_indices.push((entries.len() - 1) as u32);
            }
            for (path, target) in symlinks {
                let (parent_path, name) = match path.rsplit_once('/') {
                    Some((p, n)) => (Some(p.to_string()), n),
                    None => (None, *path),
                };
                entries.push(Entry {
                    kind: EntryKind::Symlink,
                    parent: parent_path.map(|p| dir_index[&p]).unwrap_or(u32::MAX),
                    name: intern(name),
                    mode: 0o777,
                    mtime_secs: 0,
                    mtime_nsec: 0,
                    content_hash: [0; 32],
                    blocks: Vec::new(),
                    symlink_target: intern(target),
                });
            }
            let eps: Vec<EntryPoint> = entrypoints
                .iter()
                .map(|(name, target)| EntryPoint {
                    name: intern(name),
                    target_entry: file_indices[target.parse::<usize>().unwrap()],
                    args: 0,
                    working_dir: WorkingDir::Inherit,
                    flags,
                })
                .collect();
            let manifest = Manifest {
                header: ManifestHeader {
                    version: 3,
                    entry_count: entries.len() as u32,
                    string_table_size: string_table.len() as u32,
                    entrypoint_count: eps.len() as u16,
                    default_entrypoint: 0,
                    lib_dir_count: 0,
                    name_offset: 0,
                    package_id: [0; 32],
                    prefix_hash: *blake3::hash(prefix).as_bytes(),
                    footer_hash: [0; 32],
                },
                entrypoints: eps,
                entries,
                lib_dir_offsets: Vec::new(),
                string_table,
            };
            let raw = manifest.serialize().unwrap();
            let compressed = zstd::bulk::compress(&raw, 1).unwrap();
            let manifest_offset = prefix.len() as u64;
            let payload_offset = manifest_offset + compressed.len() as u64;
            let footer = Footer {
                format_version: onelf_format::footer::FORMAT_VERSION,
                flags: Flags::STORED,
                manifest_offset,
                manifest_compressed: compressed.len() as u64,
                manifest_original: raw.len() as u64,
                payload_offset,
                payload_size: payload.len() as u64,
                dict_offset: 0,
                dict_size: 0,
                manifest_checksum: onelf_format::manifest::manifest_checksum(&raw).unwrap(),
            };
            let mut bytes = Vec::new();
            bytes.extend_from_slice(prefix);
            bytes.extend_from_slice(&compressed);
            bytes.extend_from_slice(&payload);
            footer.write_to(&mut bytes).unwrap();
            Built { bytes }
        }
    }
}

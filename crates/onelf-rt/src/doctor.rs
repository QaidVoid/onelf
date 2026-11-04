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
        Some("the package runs setuid binaries, which a user namespace disables".to_string())
    } else if !ns {
        Some("user namespaces are restricted on this host".to_string())
    } else {
        None
    };
    let fuse = || {
        let why_not_ns = no_ns.clone().or_else(|| {
            if env_set("ONELF_FUSE_NO_NAMESPACE") {
                Some("ONELF_FUSE_NO_NAMESPACE is set".to_string())
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
                format!("mounted through {} ({why})", helper.display()),
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
            "onelf-rt {}: {} (entrypoint {ep_name})",
            env!("CARGO_PKG_VERSION"),
            pkg.manifest.name()
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
        Some(helper) => line(&mut out, &format!("  mount helper: {}", helper.display())),
        None => line(
            &mut out,
            "  mount helper: none (no fusermount3 or setuid fusermount)",
        ),
    }

    let rules = read_package_file(pkg, pkgmeta::RUNTIME_RULES_FILE);
    let table = read_package_file(pkg, pkgmeta::ENTRYPOINT_SETS_FILE);
    metadata_section(&mut out, &rules, &table);

    line(&mut out, "\nExecution mode");
    let needs_setuid = read_package_file(pkg, ".onelf/needs-setuid").is_some();
    let mut chosen = None;
    for (name, verdict) in modes(pkg, ep_idx, needs_setuid, ns) {
        match verdict {
            Ok((how, places_host_files)) => {
                line(&mut out, &format!("  {name}: would run, {how}"));
                chosen = Some((name, places_host_files));
            }
            Err(why) => line(&mut out, &format!("  {name}: skipped, {why}")),
        }
    }
    if chosen.is_none() {
        line(&mut out, "  no mode can run this package here");
    }

    // The entrypoint's selection, resolved the way a launch resolves it,
    // without acquiring any of it.
    let selection = pkgmeta::validate(pkg, &ep_name).map_err(|e| e.to_string());

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
fn metadata_section(out: &mut String, rules: &Option<Vec<u8>>, table: &Option<Vec<u8>>) {
    line(out, "\nMetadata");
    if rules.is_none() && table.is_none() {
        line(out, "  records: none carried (legacy defaults)");
        return;
    }
    let decoded = table.as_deref().map(EntrypointSets::decode);
    let mut required = RuntimeRequirements::metadata(elf::RUNTIME_METADATA_VERSION);
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
    let mut carried: Vec<&str> = Vec::new();
    if rules.is_some() {
        carried.push("runtime-rules");
    }
    if table.is_some() {
        carried.push("entrypoint-sets");
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
    let mut text = format!("payload metadata version {}", required.metadata_version);
    if !required.capabilities.is_empty() {
        text.push_str(", ");
        text.push_str(&required.capabilities.to_string());
    }
    text
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
        Err(e) => line(out, &format!("  cannot read the bundled libraries: {e}")),
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
    let decision = resolve::decide(&Request {
        pkg_root: root,
        lib_dirs: &lib_dirs,
        policy,
        store: &store,
        ld_cache: &ld_cache,
        icd_dirs: resolve::ICD_DIRS,
        extra_root: None,
        set_roots: &[],
        host_libc: places_host_files,
    });
    let mut host_only = Vec::new();
    for (name, choice) in &decision.compared {
        let host = decision.winners.get(name).map(|p| p.display());
        match (choice, host) {
            (None, _) => host_only.push(name.as_str()),
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
    if let Some(interp) = &decision.host_interp {
        line(out, &format!("  loader: the host's {}", interp.display()));
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
                &format!("\nDependency sets\n  cannot resolve the selection for {ep_name}: {why}"),
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
            &format!("  selected for {ep_name}: none; no set is fetched, mounted, or extracted"),
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
    line(out, &format!("  selected for {ep_name}: {scope}"));
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
    line(out, &format!("  entrypoint: {ep_name}"));
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
    line(out, &format!("  url: {url}"));
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
        line(out, &format!("  revision: {revision}"));
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
    use std::fs::File;
    use std::path::PathBuf;

    use onelf_format::rules::RuleId;
    use onelf_format::runtime_metadata::{EntrypointSelection, EntrypointSets, SetPin};
    use onelf_format::{
        Block, Entry, EntryKind, EntryPoint, EntryPointFlags, Flags, Footer, Manifest,
        ManifestHeader, WorkingDir,
    };

    use super::{report, sets_section, view_section};
    use crate::loader;

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
            let mut string_table: Vec<u8> = vec![0];
            let mut name_offsets = Vec::new();
            for (name, _) in files {
                name_offsets.push(string_table.len() as u32);
                string_table.extend_from_slice(name.as_bytes());
                string_table.push(0);
            }
            let ep_names: Vec<u32> = entrypoints
                .iter()
                .map(|(name, _)| {
                    let offset = string_table.len() as u32;
                    string_table.extend_from_slice(name.as_bytes());
                    string_table.push(0);
                    offset
                })
                .collect();

            let mut payload = Vec::new();
            let mut entries = Vec::new();
            for (i, (_, content)) in files.iter().enumerate() {
                let offset = payload.len() as u64;
                payload.extend_from_slice(content);
                let hash = *blake3::hash(content).as_bytes();
                entries.push(Entry {
                    kind: EntryKind::File,
                    parent: u32::MAX,
                    name: name_offsets[i],
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
            }
            let eps: Vec<EntryPoint> = entrypoints
                .iter()
                .zip(ep_names)
                .map(|((_, target), name)| EntryPoint {
                    name,
                    target_entry: target.parse::<u32>().unwrap(),
                    args: 0,
                    working_dir: WorkingDir::Inherit,
                    flags: EntryPointFlags::empty(),
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

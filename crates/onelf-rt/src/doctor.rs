//! `--onelf-doctor`: what a launch of this package would do on this host,
//! worked out without running the app and without leaving anything behind.
//!
//! The mode rules repeat the order `main` tries them in. The resolver needs
//! the bundled libraries on disk, so the library directories are extracted
//! to a scratch directory that is removed again before the report ends.

use std::path::Path;

use onelf_format::resolve::{self, Gl, Request};
use onelf_format::verdef::Choice;
use onelf_format::{HostLibsPolicy, drivers};

use crate::fuse::mount;
use crate::loader::PackageData;
use crate::read_package_file;

fn env_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty() && v != "0")
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
    let needed = crate::ephemeral::tmpfs_size_for(pkg);

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
        None if !crate::ephemeral::fits_in_memory(needed) => Err(format!(
            "the {} MB package does not fit in available memory",
            needed >> 20
        )),
        None => Ok((
            "extracted to a tmpfs in a private user namespace".to_string(),
            true,
        )),
    };
    let rundir = || match crate::paths::private_dir() {
        None => Err("there is no private runtime directory".to_string()),
        Some(dir) => match crate::rundir::free_space(&dir) {
            Some(free) if free < needed => Err(format!(
                "{} has {} MB free, the package needs {} MB",
                dir.display(),
                free >> 20,
                needed >> 20
            )),
            _ => Ok((format!("extracted to {}", dir.display()), true)),
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
            Some(f) if f != name => Err(format!("ONELF_MODE={f}")),
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

/// Print the report. Nothing is run, and nothing it creates outlives it.
pub fn run(pkg: &mut PackageData, ep_idx: usize) {
    let ep_name = pkg
        .manifest
        .get_string(pkg.manifest.entrypoints[ep_idx].name)
        .to_string();
    println!(
        "onelf-rt {}: {} (entrypoint {ep_name})",
        env!("CARGO_PKG_VERSION"),
        pkg.manifest.name()
    );

    println!("\nHost");
    let ns = mount::namespace_usable();
    println!(
        "  user namespaces: {}",
        if ns { "usable" } else { "restricted" }
    );
    match mount::helper() {
        Some(helper) => println!("  mount helper: {}", helper.display()),
        None => println!("  mount helper: none (no fusermount3 or setuid fusermount)"),
    }

    let needs_setuid = read_package_file(pkg, ".onelf/needs-setuid").is_some();
    println!("\nExecution mode");
    let mut chosen = None;
    for (name, verdict) in modes(pkg, ep_idx, needs_setuid, ns) {
        match verdict {
            Ok((how, places_host_files)) => {
                println!("  {name}: would run, {how}");
                chosen = Some((name, places_host_files));
            }
            Err(why) => println!("  {name}: skipped, {why}"),
        }
    }
    if chosen.is_none() {
        println!("  no mode can run this package here");
    }

    let lib_dirs: Vec<String> = pkg
        .manifest
        .lib_dirs()
        .iter()
        .map(|d| d.to_string())
        .collect();
    if let Some((name, places_host_files)) = chosen
        && name != "memfd"
        && !lib_dirs.is_empty()
    {
        libraries(pkg, &lib_dirs, places_host_files);
    }
    sets(pkg);
    update(pkg);
}

fn libraries(pkg: &mut PackageData, lib_dirs: &[String], places_host_files: bool) {
    println!("\nLibraries");
    let disabled = env_set("ONELF_NO_RESOLVER");
    let policy = if disabled {
        HostLibsPolicy::Never
    } else {
        HostLibsPolicy::from_flags(pkg.footer.flags)
    };
    println!(
        "  host-libs: {}{}",
        policy.as_str(),
        if disabled { " (ONELF_NO_RESOLVER)" } else { "" }
    );

    let Some(scratch) =
        crate::paths::private_dir().map(|d| d.join(format!("onelf-doctor-{}", std::process::id())))
    else {
        println!("  no private runtime directory to read the libraries in");
        return;
    };
    let in_lib_dir = |rel: &Path| {
        rel.parent()
            .is_some_and(|p| lib_dirs.iter().any(|d| p == Path::new(d)))
    };
    let extracted = std::fs::create_dir(&scratch)
        .and_then(|()| crate::cache::extract_matching(pkg, &scratch, in_lib_dir));
    match extracted {
        Ok(()) => verdicts(pkg, &scratch, lib_dirs, policy, places_host_files),
        Err(e) => println!("  cannot read the bundled libraries: {e}"),
    }
    let _ = std::fs::remove_dir_all(&scratch);
}

fn verdicts(
    pkg: &mut PackageData,
    root: &Path,
    lib_dirs: &[String],
    policy: HostLibsPolicy,
    places_host_files: bool,
) {
    let lib_dirs: Vec<&str> = lib_dirs.iter().map(String::as_str).collect();
    let ld_cache = drivers::cache_file();
    let gl = resolve::gl_situation(root, &lib_dirs, &[], &ld_cache, resolve::ICD_DIRS);
    let gl_text = match gl {
        Gl::Bundled => "bundled",
        Gl::Host => "from the host",
        Gl::Absent => "none on the host or in the bundle",
    };
    println!("  GL stack: {gl_text}");
    let pin = read_package_file(pkg, crate::platform::PIN_FILE)
        .and_then(|b| crate::platform::parse_pin(&String::from_utf8_lossy(&b)));
    if let Some(pin) = &pin {
        let wanted = if gl == Gl::Absent {
            "the launch uses it"
        } else {
            "not needed here"
        };
        println!(
            "  pinned GL build: {}, {}; {wanted}",
            pin.label,
            stored(pin)
        );
    }

    if policy == HostLibsPolicy::Never {
        return;
    }
    let Some(store) = crate::paths::resolve_store(&pkg.manifest.header.package_id) else {
        println!("  no private runtime directory for the resolver's store");
        return;
    };
    if !places_host_files {
        println!("  this mode cannot put a host file over a bundled one, so glibc stays bundled");
    }
    if gl == Gl::Absent && pin.is_some() {
        println!("  the pinned build's libraries are not weighed below");
    }
    if read_package_file(pkg, crate::sets::SETS_FILE).is_some() {
        println!("  the dependency sets' libraries are not weighed below");
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
            (Some(Choice::Host), Some(host)) => println!("  {name}: the host's is newer, {host}"),
            (Some(Choice::Host), None) => println!("  {name}: the host's is newer"),
            (Some(Choice::Bundle), _) => println!("  {name}: bundled copy"),
            (Some(Choice::Incomparable), _) => {
                println!("  {name}: incomparable, bundled copy kept")
            }
        }
    }
    if !host_only.is_empty() {
        println!(
            "  from the host, not bundled ({}): {}",
            host_only.len(),
            host_only.join(", ")
        );
    }
    if decision.compared.is_empty() {
        println!("  nothing to compare: the host offers no driver libraries");
    }
    if let Some(interp) = &decision.host_interp {
        println!("  loader: the host's {}", interp.display());
    }
    if let Some(farm) = &decision.farm {
        let state = if farm.is_dir() {
            "exists"
        } else {
            "a launch creates it"
        };
        println!("  link farm: {} ({state})", farm.display());
    }
}

/// Whether the store holds the build `pin` names, or where it comes from.
fn stored(pin: &crate::platform::Pin) -> String {
    match crate::platform::stored_path(pin) {
        Ok(file) if file.is_file() => format!("stored at {}", file.display()),
        _ if env_set("ONELF_NO_PLATFORM_FETCH") => {
            "not stored, and ONELF_NO_PLATFORM_FETCH forbids fetching it".to_string()
        }
        _ => format!("not stored, a launch fetches {}", pin.url),
    }
}

fn sets(pkg: &mut PackageData) {
    let Some(record) = read_package_file(pkg, crate::sets::SETS_FILE) else {
        return;
    };
    println!("\nDependency sets");
    for pin in crate::sets::parse(&String::from_utf8_lossy(&record)) {
        println!("  {}: {}", pin.label, stored(&pin));
    }
}

fn update(pkg: &mut PackageData) {
    println!("\nSelf-update");
    let text = |pkg: &mut PackageData, path: &str| {
        read_package_file(pkg, path).map(|b| String::from_utf8_lossy(&b).trim().to_string())
    };
    let Some(url) = text(pkg, ".onelf/update-url") else {
        println!("  not configured");
        return;
    };
    println!("  url: {url}");
    match read_package_file(pkg, ".onelf/update-key") {
        Some(keys) => println!("  signing keys: {}", keys.len() / 32),
        None => println!("  signing keys: none, so --onelf-update refuses"),
    }
    if let Some(revision) = text(pkg, ".onelf/update-revision") {
        println!("  revision: {revision}");
    }
    if cfg!(feature = "update") {
        println!("  updater: in this runtime");
    } else {
        println!("  updater: not in this runtime, so the update flags reach the app");
    }
}

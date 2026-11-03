//! Selected provider compatibility, checked before an application runs.
//!
//! A selected set's build is verified bytes at acquisition, but nothing in
//! its pin says its objects can run beside this package: an artifact
//! published before provider indexes existed carries no provenance, a pin
//! is not an architecture, and `.onelf/sonames` never claimed to be either.
//! So the launch derives everything it needs from the objects themselves
//! and from the application's bundled libc and loader, the same bounded ELF
//! inspection the packer audits with. Old set artifacts need no
//! republishing: whatever they carry is read from their verified trees as
//! they stand.
//!
//! Two gates sit on the launch path. Preparation validates each obtained
//! set while it is acquired, so an object whose ELF data is truncated or
//! malformed ends the launch there, before any further pin, namespace or
//! mount exists. The launch itself then compares every selected object
//! against what this launch can actually satisfy: the bundled libc and
//! loader pair and the selected sets together. An object for another
//! machine, one that mixes an unrelated libc family into the launch, and
//! one that requires a symbol version nothing in the launch defines all
//! end the launch here, before the resolver edits a tree, the environment
//! is touched, or the application executes. A newer host is never consulted
//! to repair a toolkit set.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use onelf_format::verdef::{self, ElfClass, ElfFacts, LibcFamily};

/// The most provider objects one launch inspects per set. Beyond this the
/// launch fails with a bounded diagnostic instead of assuming anything
/// about the rest of the tree.
const MAX_OBJECTS_PER_SET: usize = 16384;

/// The most findings one launch reports. Every finding still fails the
/// launch; the cap keeps one flooded selection from flooding the terminal.
const SHOWN_FINDINGS: usize = 4;

/// One inspected provider object: where it sits in its set's tree and the
/// bounded facts read from its bytes.
#[derive(Clone, Debug)]
pub(crate) struct ObjectFacts {
    /// The object's path relative to its set's root.
    pub(crate) rel: PathBuf,
    /// What its ELF metadata says.
    pub(crate) facts: ElfFacts,
}

/// One selected set's inspected objects, named for the diagnostics a
/// launch rejects the selection with.
#[derive(Clone, Debug)]
pub(crate) struct ProviderFacts {
    /// The set table's declaration the objects belong to.
    pub(crate) label: String,
    /// Every object the set's library directories carry.
    pub(crate) objects: Vec<ObjectFacts>,
}

/// The selected sets this launch prepared, held for the compatibility
/// check so the objects are read once per launch.
static SELECTED: OnceLock<Vec<ProviderFacts>> = OnceLock::new();

/// Inspect one obtained set and keep its objects' facts.
///
/// Every file in the set's library directories that carries ELF data is
/// read through the shared bounded inspection. A file without ELF data is
/// not provider data and is left alone. ELF data that cannot be read in
/// full is the error: a truncated or malformed object is a launch failure
/// naming the set and the object, never a silent pass. The facts are
/// returned so the launch's compatibility check reads each object once.
pub(crate) fn prepare(label: &str, root: &Path) -> Result<ProviderFacts, String> {
    let mut objects = Vec::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    for dir in crate::sets::lib_dirs(root) {
        let entries = fs::read_dir(&dir)
            .map_err(|e| format!("{}: {e}", display_rel(&dir, root).display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            // Links are followed: a set keeps its plain and versioned
            // sonames as links onto one object, which is inspected once.
            if !path.is_file() {
                continue;
            }
            if !has_elf_magic(&path) {
                continue;
            }
            let key = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if !seen.insert(key) {
                continue;
            }
            if objects.len() >= MAX_OBJECTS_PER_SET {
                return Err(format!(
                    "{}: the set carries more objects than a launch can validate \
                     ({MAX_OBJECTS_PER_SET})",
                    display_rel(&path, root).display()
                ));
            }
            let facts = verdef::read_facts(&path)
                .map_err(|e| format!("{}: {e}", display_rel(&path, root).display()))?;
            objects.push(ObjectFacts {
                rel: display_rel(&path, root),
                facts,
            });
        }
    }
    Ok(ProviderFacts {
        label: label.to_string(),
        objects,
    })
}

/// Hold the facts of the sets this launch selected, in acquisition order.
pub(crate) fn record_selected(facts: Vec<ProviderFacts>) {
    let _ = SELECTED.set(facts);
}

/// The facts of this launch's selected sets, in acquisition order.
///
/// A launch whose preparation did not record them reads the roots again,
/// so the check never mistakes an unrecorded selection for a compatible
/// one.
fn facts_for_launch() -> Vec<ProviderFacts> {
    if let Some(recorded) = SELECTED.get()
        && !recorded.is_empty()
    {
        return recorded.clone();
    }
    let labels = crate::sets::pins();
    let roots = crate::sets::roots();
    let mut facts = Vec::new();
    for (pin, root) in labels.iter().zip(roots.iter()) {
        match prepare(&pin.label, root) {
            Ok(fact) => facts.push(fact),
            Err(why) => {
                eprintln!("onelf-rt: dependency set {}: {why}", pin.label);
                std::process::exit(1);
            }
        }
    }
    facts
}

/// Whether a file starts with ELF data. A shorter read, like any other
/// file whose head is not ELF, means there is nothing to inspect.
fn has_elf_magic(path: &Path) -> bool {
    let mut magic = [0u8; 4];
    fs::File::open(path)
        .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut magic))
        .is_ok_and(|()| magic == *b"\x7fELF")
}

/// A path relative to the set root when it belongs to one, so diagnostics
/// name objects the way the set is laid out.
fn display_rel(path: &Path, root: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}

/// What this launch can satisfy a provider's needs with: the bundled libc
/// and loader pair.
struct LaunchPair {
    class: ElfClass,
    machine: u16,
    family: LibcFamily,
    /// The GNU symbol versions the pair defines, including the loader's.
    versions: BTreeSet<String>,
}

/// Establish the application's libc and loader pair from its own tree.
///
/// The loader recorded beside the entrypoint at pack time comes first,
/// then every loader and libc the package's library directories carry,
/// then the entrypoint itself for a package that bundles no libc. Files
/// that hold no ELF data, a linker script or a script entrypoint, are not
/// the pair and are passed over; only a real object establishes the
/// architecture. The pair's definitions join the versions a provider's
/// requirements may be answered from, and its libc family is the family
/// evidence its members' own names carry.
fn launch_pair(
    pkg_root: &Path,
    lib_dirs: &[&str],
    target: &Path,
    interp_rel: Option<&str>,
) -> Option<LaunchPair> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(rel) = interp_rel {
        let path = pkg_root.join(rel);
        if path.is_file() {
            candidates.push(path);
        }
    }
    for dir in lib_dirs {
        let Ok(entries) = fs::read_dir(pkg_root.join(dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let is_loader = name.starts_with("ld-linux")
                || name.starts_with("ld-musl-")
                || name == "ld64.so.1"
                || name == "ld64.so.2";
            if is_loader || name.starts_with("libc.") {
                let path = entry.path();
                if path.is_file() {
                    candidates.push(path);
                }
            }
        }
    }
    candidates.push(target.to_path_buf());

    let mut pair: Option<LaunchPair> = None;
    for path in candidates {
        let Ok(facts) = verdef::read_facts(&path) else {
            continue;
        };
        let family = name_family(&path).unwrap_or(facts.libc_family);
        match pair.as_mut() {
            None => {
                pair = Some(LaunchPair {
                    class: facts.class,
                    machine: facts.machine,
                    family,
                    versions: facts.defined_versions.clone(),
                });
            }
            Some(pair) => {
                pair.versions.extend(facts.defined_versions.iter().cloned());
                pair.family = combine_family(pair.family, family);
            }
        }
    }
    pair
}

/// The libc family a bundled pair member's own name evidences: the name
/// the file carries is family evidence its contents may not repeat.
fn name_family(path: &Path) -> Option<LibcFamily> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    if name.starts_with("ld-linux") || name == "ld64.so.1" || name == "ld64.so.2" {
        Some(LibcFamily::Glibc)
    } else if name.starts_with("ld-musl-") {
        Some(LibcFamily::Musl)
    } else if name == "libc.so.6" {
        Some(LibcFamily::Glibc)
    } else if name.starts_with("libc.musl-") {
        Some(LibcFamily::Musl)
    } else {
        None
    }
}

/// Family evidence that agrees is kept, and evidence that disagrees is
/// recorded as the conflict it is rather than resolved by order.
fn combine_family(one: LibcFamily, two: LibcFamily) -> LibcFamily {
    match (one, two) {
        (LibcFamily::Unknown, other) | (other, LibcFamily::Unknown) => other,
        (a, b) if a == b => a,
        _ => LibcFamily::Conflicting,
    }
}

/// Check this launch's selected providers against what the launch can
/// satisfy, before the resolver, the environment, or the application runs.
///
/// `pkg_root`, `lib_dirs`, `target` and `interp_rel` establish the
/// application's bundled libc and loader pair; `entrypoint` names the
/// selection in every diagnostic. A selection carrying no provider data
/// passes, as does one whose objects the launch can account for.
pub(crate) fn validate_selected(
    pkg_root: &Path,
    lib_dirs: &[&str],
    target: &Path,
    interp_rel: Option<&str>,
    entrypoint: &str,
) -> Result<(), String> {
    let providers = facts_for_launch();
    if providers.is_empty() {
        return Ok(());
    }
    let pair = launch_pair(pkg_root, lib_dirs, target, interp_rel);
    let universe = requirement_universe(&providers, pair.as_ref());
    check_selection(entrypoint, &providers, pair.as_ref(), &universe)
}

/// Check gathered facts against an established pair and version universe.
fn check_selection(
    entrypoint: &str,
    providers: &[ProviderFacts],
    pair: Option<&LaunchPair>,
    universe: &BTreeSet<String>,
) -> Result<(), String> {
    let mut findings = Vec::new();
    for provider in providers {
        for object in &provider.objects {
            if let Some(pair) = pair {
                architecture_findings(entrypoint, provider, object, pair, &mut findings);
            }
            requirement_findings(entrypoint, provider, object, universe, &mut findings);
        }
    }
    if findings.is_empty() {
        return Ok(());
    }
    let mut message = format!("entrypoint {entrypoint} cannot run with its selected sets:");
    for finding in findings.iter().take(SHOWN_FINDINGS) {
        message.push_str("\n  ");
        message.push_str(finding);
    }
    let hidden = findings.len().saturating_sub(SHOWN_FINDINGS);
    if hidden > 0 {
        message.push_str(&format!("\n  and {hidden} more findings"));
    }
    Err(message)
}

/// The versions a provider's requirements may be answered from: the
/// bundled pair's definitions and every selected set's own definitions.
fn requirement_universe(
    providers: &[ProviderFacts],
    pair: Option<&LaunchPair>,
) -> BTreeSet<String> {
    let mut universe: BTreeSet<String> = pair.map(|pair| pair.versions.clone()).unwrap_or_default();
    for provider in providers {
        for object in &provider.objects {
            universe.extend(object.facts.defined_versions.iter().cloned());
        }
    }
    universe
}

/// The architecture and libc-family findings one provider object makes.
fn architecture_findings(
    entrypoint: &str,
    provider: &ProviderFacts,
    object: &ObjectFacts,
    pair: &LaunchPair,
    findings: &mut Vec<String>,
) {
    let what = |reason: String| {
        format!(
            "entrypoint {entrypoint}: set {}: {}: {}",
            provider.label,
            object.rel.display(),
            reason
        )
    };
    if object.facts.class != pair.class || object.facts.machine != pair.machine {
        findings.push(what(format!(
            "ELF class {:?} machine {}, which this launch's class {:?} machine {} cannot load",
            object.facts.class, object.facts.machine, pair.class, pair.machine
        )));
    }
    match (pair.family, object.facts.libc_family) {
        (LibcFamily::Glibc, LibcFamily::Musl) | (LibcFamily::Musl, LibcFamily::Glibc) => {
            findings.push(what(format!(
                "needs a {} libc, which this launch's {} libc cannot supply",
                match object.facts.libc_family {
                    LibcFamily::Musl => "musl",
                    _ => "glibc",
                },
                match pair.family {
                    LibcFamily::Musl => "musl",
                    _ => "glibc",
                }
            )));
        }
        (_, LibcFamily::Conflicting) => {
            findings.push(what("names both libc families".to_string()));
        }
        _ => {}
    }
}

/// The version findings one provider object makes: a mandatory symbol
/// version nothing in the launch defines. Weak requirements are the
/// loader's own discretion and are not enforced here.
fn requirement_findings(
    entrypoint: &str,
    provider: &ProviderFacts,
    object: &ObjectFacts,
    universe: &BTreeSet<String>,
    findings: &mut Vec<String>,
) {
    for (dependency, required) in &object.facts.required_versions {
        for version in required {
            if version.is_weak() || universe.contains(&version.name) {
                continue;
            }
            findings.push(format!(
                "entrypoint {entrypoint}: set {}: {}: requires {} from {}, which neither \
                 the bundled libc and loader nor the selected sets define",
                provider.label,
                object.rel.display(),
                version.name,
                dependency
            ));
        }
    }
}

/// Minimal ELF64 objects for the tests: a string table, a dynamic section
/// naming `soname` and `needed`, version definitions over `definitions`
/// (the first entry is the base version) and version requirements over
/// `requirements`, whose file names must appear in `needed`.
#[cfg(test)]
pub(crate) mod fixture {
    const SHT_STRTAB: u32 = 3;
    const SHT_DYNAMIC: u32 = 6;
    const SHT_GNU_VERDEF: u32 = 0x6fff_fffd;
    const SHT_GNU_VERNEED: u32 = 0x6fff_fffe;
    const DT_NEEDED: u64 = 1;
    const DT_SONAME: u64 = 14;
    const DT_VERNEED: u64 = 0x6fff_fffe;
    const DT_VERNEEDNUM: u64 = 0x6fff_ffff;
    const VERDEF_ENTRY: usize = 20;
    const VERDAUX_ENTRY: usize = 8;

    pub(crate) fn elf64(
        machine: u16,
        soname: Option<&str>,
        needed: &[&str],
        definitions: &[(u16, &str)],
        requirements: &[(&str, &[(u16, &str)])],
    ) -> Vec<u8> {
        let mut strings: Vec<u8> = vec![0];
        let intern = |name: &str, strings: &mut Vec<u8>| {
            let at = strings.len() as u32;
            strings.extend_from_slice(name.as_bytes());
            strings.push(0);
            at
        };
        let soname_offset = soname.map(|name| intern(name, &mut strings));
        let needed_offsets: Vec<u32> = needed.iter().map(|n| intern(n, &mut strings)).collect();
        let definition_offsets: Vec<u32> = definitions
            .iter()
            .map(|(_, n)| intern(n, &mut strings))
            .collect();

        let mut dynamic = Vec::new();
        for offset in &needed_offsets {
            dynamic.extend_from_slice(&DT_NEEDED.to_le_bytes());
            dynamic.extend_from_slice(&(*offset as u64).to_le_bytes());
        }
        if let Some(offset) = soname_offset {
            dynamic.extend_from_slice(&DT_SONAME.to_le_bytes());
            dynamic.extend_from_slice(&(offset as u64).to_le_bytes());
        }
        if !requirements.is_empty() {
            dynamic.extend_from_slice(&DT_VERNEED.to_le_bytes());
            dynamic.extend_from_slice(&0x5000u64.to_le_bytes());
            dynamic.extend_from_slice(&DT_VERNEEDNUM.to_le_bytes());
            dynamic.extend_from_slice(&(requirements.len() as u64).to_le_bytes());
        }
        dynamic.extend_from_slice(&[0; 16]);

        let mut verdef = Vec::new();
        for (i, (flags, _)) in definitions.iter().enumerate() {
            let last = i + 1 == definitions.len();
            verdef.extend_from_slice(&1u16.to_le_bytes());
            verdef.extend_from_slice(&flags.to_le_bytes());
            verdef.extend_from_slice(&(i as u16).to_le_bytes());
            verdef.extend_from_slice(&1u16.to_le_bytes());
            verdef.extend_from_slice(&0u32.to_le_bytes());
            verdef.extend_from_slice(&(VERDEF_ENTRY as u32).to_le_bytes());
            let next = if last {
                0
            } else {
                (VERDEF_ENTRY + VERDAUX_ENTRY) as u32
            };
            verdef.extend_from_slice(&next.to_le_bytes());
            verdef.extend_from_slice(&definition_offsets[i].to_le_bytes());
            verdef.extend_from_slice(&0u32.to_le_bytes());
        }

        let mut verneed = Vec::new();
        let verneed_count = requirements.len() as u32;
        for (i, (file, versions)) in requirements.iter().enumerate() {
            let start = verneed.len();
            let size = 16 + 16 * versions.len();
            verneed.resize(start + size, 0);
            verneed[start..start + 2].copy_from_slice(&1u16.to_le_bytes());
            verneed[start + 2..start + 4].copy_from_slice(&(versions.len() as u16).to_le_bytes());
            verneed[start + 4..start + 8]
                .copy_from_slice(&intern(file, &mut strings).to_le_bytes());
            verneed[start + 8..start + 12].copy_from_slice(&16u32.to_le_bytes());
            if i + 1 < requirements.len() {
                verneed[start + 12..start + 16].copy_from_slice(&(size as u32).to_le_bytes());
            }
            for (j, (flags, name)) in versions.iter().enumerate() {
                let at = start + 16 + j * 16;
                verneed[at + 4..at + 6].copy_from_slice(&flags.to_le_bytes());
                verneed[at + 6..at + 8].copy_from_slice(&((j + 2) as u16).to_le_bytes());
                verneed[at + 8..at + 12].copy_from_slice(&intern(name, &mut strings).to_le_bytes());
                if j + 1 < versions.len() {
                    verneed[at + 12..at + 16].copy_from_slice(&16u32.to_le_bytes());
                }
            }
        }

        let shoff = 64usize;
        let shentsize = 64usize;
        let mut sections: Vec<(u32, Vec<u8>, u64, u32, u32)> = vec![
            (SHT_STRTAB, strings.clone(), 0, 0, 0),
            (SHT_DYNAMIC, dynamic, 0, 1, 0),
        ];
        if !verdef.is_empty() {
            sections.push((SHT_GNU_VERDEF, verdef, 0, 1, 0));
        }
        if !verneed.is_empty() {
            sections.push((SHT_GNU_VERNEED, verneed, 0x5000, 1, verneed_count));
        }
        let shnum = sections.len() + 1;
        let mut body_at = shoff + shentsize * shnum;
        let mut offsets = Vec::new();
        for (_, bytes, _, _, _) in &sections {
            offsets.push(body_at as u64);
            body_at += bytes.len();
        }
        let mut out = vec![0u8; body_at];
        out[0..4].copy_from_slice(b"\x7fELF");
        out[4] = 2;
        out[5] = 1;
        out[16..18].copy_from_slice(&3u16.to_le_bytes());
        out[18..20].copy_from_slice(&machine.to_le_bytes());
        out[40..48].copy_from_slice(&(shoff as u64).to_le_bytes());
        out[58..60].copy_from_slice(&(shentsize as u16).to_le_bytes());
        out[60..62].copy_from_slice(&(shnum as u16).to_le_bytes());
        for (i, (kind, bytes, address, link, info)) in sections.iter().enumerate() {
            let at = shoff + (i + 1) * shentsize;
            out[at + 4..at + 8].copy_from_slice(&kind.to_le_bytes());
            out[at + 16..at + 24].copy_from_slice(&address.to_le_bytes());
            out[at + 24..at + 32].copy_from_slice(&offsets[i].to_le_bytes());
            out[at + 32..at + 40].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
            out[at + 40..at + 44].copy_from_slice(&link.to_le_bytes());
            out[at + 44..at + 48].copy_from_slice(&info.to_le_bytes());
            let at = offsets[i] as usize;
            out[at..at + bytes.len()].copy_from_slice(bytes);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::elf64;
    use super::*;

    /// A sandbox tree with a package root and set roots written to disk.
    struct Tree {
        root: PathBuf,
    }

    impl Tree {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "onelf-providers-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            Tree { root }
        }

        fn write(&self, rel: &str, bytes: &[u8]) {
            let path = self.root.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, bytes).unwrap();
        }

        fn package(&self) -> PathBuf {
            self.root.join("pkg")
        }

        fn set(&self, name: &str) -> PathBuf {
            self.root.join(name)
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    const VER_FLG_BASE: u16 = 1;

    /// The bundled pair member: a glibc-shaped libc for machine 62 that
    /// defines GLIBC_2.2.5 and nothing newer.
    fn bundled_libc() -> Vec<u8> {
        elf64(
            62,
            Some("libc.so.6"),
            &["ld-linux-x86-64.so.2"],
            &[(VER_FLG_BASE, "libc.so.6"), (0, "GLIBC_2.2.5")],
            &[],
        )
    }

    /// One selected set with a single object, prepared from disk.
    fn set_with(
        tree: &Tree,
        name: &str,
        machine: u16,
        needed: &[&str],
        definitions: &[(u16, &str)],
        requirements: &[(&str, &[(u16, &str)])],
    ) -> ProviderFacts {
        tree.write(
            &format!("{name}/lib/libqt.so.1"),
            &elf64(
                machine,
                Some("libqt.so.1"),
                needed,
                definitions,
                requirements,
            ),
        );
        prepare(name, &tree.set(name)).unwrap()
    }

    /// The launch check over a package whose tree is `pkg`, using the same
    /// pair discovery the launch runs, without the process-wide record.
    fn check(
        entrypoint: &str,
        providers: &[ProviderFacts],
        pkg: &Path,
        lib_dirs: &[&str],
        target: &str,
        interp_rel: Option<&str>,
    ) -> Result<(), String> {
        let pair = launch_pair(pkg, lib_dirs, &pkg.join(target), interp_rel);
        let universe = requirement_universe(providers, pair.as_ref());
        check_selection(entrypoint, providers, pair.as_ref(), &universe)
    }

    /// The common shape: a package bundling the glibc pair and a set.
    fn with_bundled_libc(tag: &str) -> Tree {
        let tree = Tree::new(tag);
        tree.write("pkg/lib/libc.so.6", &bundled_libc());
        tree.write("pkg/bin/app", b"#!/bin/sh\nexit 0\n");
        tree
    }

    #[test]
    fn a_truncated_object_ends_preparation_naming_the_set_and_object() {
        let tree = Tree::new("truncated");
        // Cutting the section table's span leaves valid-looking ELF data
        // whose sections lie outside what survived.
        let mut bytes = elf64(62, Some("libcut.so.1"), &["libc.so.6"], &[], &[]);
        bytes.truncate(bytes.len() - 8);
        tree.write("set/lib/libcut.so.1", &bytes);
        let why = prepare("qt", &tree.set("set")).unwrap_err();
        assert!(why.contains("lib/libcut.so.1"), "{why}");
        assert!(why.contains("ELF"), "{why}");

        // Cutting the head between magic and header is refused too.
        let bytes = elf64(62, Some("libcut.so.1"), &["libc.so.6"], &[], &[]);
        tree.write("set/lib/libhead.so.1", &bytes[..20]);
        let why = prepare("qt", &tree.set("set")).unwrap_err();
        assert!(why.contains("lib/libhead.so.1"), "{why}");
    }

    #[test]
    fn a_file_without_elf_data_is_not_provider_data() {
        let tree = Tree::new("plain");
        tree.write("set/lib/plugin/readme.txt", b"notes\n");
        tree.write("set/lib/plugin/libp.so", b"#!/bin/sh\nexit 0\n");
        let facts = prepare("qt", &tree.set("set")).unwrap();
        assert!(facts.objects.is_empty());
    }

    #[test]
    fn a_head_cut_past_the_magic_is_not_elf_data_either() {
        let tree = Tree::new("short");
        tree.write("set/lib/libcut.so.1", b"\x7fE");
        let facts = prepare("qt", &tree.set("set")).unwrap();
        assert!(facts.objects.is_empty());
    }

    #[test]
    fn a_valid_object_is_read_whole_with_its_versions() {
        let tree = Tree::new("valid");
        tree.write(
            "set/lib/libqt.so.1",
            &elf64(
                62,
                Some("libqt.so.1"),
                &["libc.so.6"],
                &[(VER_FLG_BASE, "libqt.so.1"), (0, "QT_1.0")],
                &[("libc.so.6", &[(0, "GLIBC_2.2.5")])],
            ),
        );
        let facts = prepare("qt", &tree.set("set")).unwrap();
        assert_eq!(facts.objects.len(), 1);
        assert_eq!(facts.objects[0].rel, PathBuf::from("lib/libqt.so.1"));
        assert_eq!(facts.objects[0].facts.machine, 62);
        assert_eq!(
            facts.objects[0]
                .facts
                .required_versions
                .get("libc.so.6")
                .map(|v| v[0].name.as_str()),
            Some("GLIBC_2.2.5")
        );
    }

    #[test]
    fn a_matching_provider_passes_the_launch_check() {
        let tree = with_bundled_libc("ok");
        let providers = [set_with(
            &tree,
            "qt",
            62,
            &["libc.so.6"],
            &[(VER_FLG_BASE, "libqt.so.1")],
            &[("libc.so.6", &[(0, "GLIBC_2.2.5")])],
        )];
        check(
            "app",
            &providers,
            &tree.package(),
            &["lib"],
            "bin/app",
            None,
        )
        .unwrap();
    }

    #[test]
    fn a_wrong_architecture_provider_names_the_object() {
        let tree = with_bundled_libc("arch");
        let providers = [set_with(
            &tree,
            "qt",
            3,
            &[],
            &[(VER_FLG_BASE, "libqt.so.1")],
            &[],
        )];
        let why = check(
            "app",
            &providers,
            &tree.package(),
            &["lib"],
            "bin/app",
            None,
        )
        .unwrap_err();
        assert!(why.contains("set qt"), "{why}");
        assert!(why.contains("lib/libqt.so.1"), "{why}");
        assert!(why.contains("machine 3"), "{why}");
        assert!(why.contains("machine 62"), "{why}");
        assert!(why.contains("entrypoint app"), "{why}");
    }

    #[test]
    fn a_version_beyond_the_bundled_libc_ends_the_launch() {
        let tree = with_bundled_libc("version");
        let providers = [set_with(
            &tree,
            "qt",
            62,
            &["libc.so.6"],
            &[(VER_FLG_BASE, "libqt.so.1")],
            &[("libc.so.6", &[(0, "GLIBC_2.999")])],
        )];
        let why = check(
            "app",
            &providers,
            &tree.package(),
            &["lib"],
            "bin/app",
            None,
        )
        .unwrap_err();
        assert!(why.contains("GLIBC_2.999"), "{why}");
        assert!(why.contains("from libc.so.6"), "{why}");
        assert!(why.contains("entrypoint app"), "{why}");
    }

    #[test]
    fn a_weak_requirement_is_not_enforced() {
        let tree = with_bundled_libc("weak");
        let providers = [set_with(
            &tree,
            "qt",
            62,
            &["libc.so.6"],
            &[(VER_FLG_BASE, "libqt.so.1")],
            &[("libc.so.6", &[(2, "GLIBC_2.999")])],
        )];
        check(
            "app",
            &providers,
            &tree.package(),
            &["lib"],
            "bin/app",
            None,
        )
        .unwrap();
    }

    #[test]
    fn a_version_another_selected_set_defines_is_satisfied() {
        let tree = with_bundled_libc("cross-set");
        tree.write(
            "other/lib/libother.so.1",
            &elf64(
                62,
                Some("libother.so.1"),
                &[],
                &[(VER_FLG_BASE, "libother.so.1"), (0, "QT_1.0")],
                &[],
            ),
        );
        let providers = [
            set_with(
                &tree,
                "qt",
                62,
                &["libother.so.1"],
                &[(VER_FLG_BASE, "libqt.so.1")],
                &[("libother.so.1", &[(0, "QT_1.0")])],
            ),
            prepare("other", &tree.set("other")).unwrap(),
        ];
        check(
            "app",
            &providers,
            &tree.package(),
            &["lib"],
            "bin/app",
            None,
        )
        .unwrap();
    }

    #[test]
    fn a_libc_family_the_launch_does_not_carry_is_refused() {
        let tree = with_bundled_libc("family");
        let providers = [set_with(
            &tree,
            "qt",
            62,
            &["libc.musl-x86_64.so.1"],
            &[],
            &[],
        )];
        let why = check(
            "app",
            &providers,
            &tree.package(),
            &["lib"],
            "bin/app",
            None,
        )
        .unwrap_err();
        assert!(why.contains("musl"), "{why}");
        assert!(why.contains("glibc"), "{why}");
    }

    #[test]
    fn an_object_naming_both_libc_families_is_refused() {
        let tree = with_bundled_libc("conflict");
        let providers = [set_with(
            &tree,
            "qt",
            62,
            &["libc.so.6", "libc.musl-x86_64.so.1"],
            &[],
            &[],
        )];
        let why = check(
            "app",
            &providers,
            &tree.package(),
            &["lib"],
            "bin/app",
            None,
        )
        .unwrap_err();
        assert!(why.contains("both libc families"), "{why}");
    }

    #[test]
    fn a_package_without_a_bundled_pair_answers_against_its_sets_alone() {
        let tree = Tree::new("no-pair");
        tree.write("pkg/bin/app", b"#!/bin/sh\nexit 0\n");
        let providers = [set_with(
            &tree,
            "qt",
            62,
            &["libc.so.6"],
            &[(VER_FLG_BASE, "libqt.so.1")],
            &[("libc.so.6", &[(0, "GLIBC_2.2.5")])],
        )];
        check(
            "app",
            &providers,
            &tree.package(),
            &["lib"],
            "bin/app",
            None,
        )
        .unwrap_err();
        let satisfied = [set_with(
            &tree,
            "self",
            62,
            &[],
            &[(VER_FLG_BASE, "libqt.so.1"), (0, "QT_1.0")],
            &[],
        )];
        check(
            "app",
            &satisfied,
            &tree.package(),
            &["lib"],
            "bin/app",
            None,
        )
        .unwrap();
    }

    #[test]
    fn a_script_entrypoint_establishes_no_pair_so_architecture_is_skipped() {
        let tree = Tree::new("script");
        tree.write("pkg/bin/app", b"#!/bin/sh\nexit 0\n");
        let providers = [set_with(
            &tree,
            "qt",
            3,
            &[],
            &[(VER_FLG_BASE, "libqt.so.1")],
            &[],
        )];
        check("app", &providers, &tree.package(), &[], "bin/app", None).unwrap();
    }

    #[test]
    fn the_recorded_interp_establishes_the_pair_for_the_architecture_check() {
        let tree = Tree::new("interp");
        tree.write(
            "pkg/lib/ld-linux-x86-64.so.2",
            &elf64(62, None, &[], &[], &[]),
        );
        tree.write("pkg/bin/app", b"#!/bin/sh\nexit 0\n");
        let providers = [set_with(
            &tree,
            "qt",
            3,
            &[],
            &[(VER_FLG_BASE, "libqt.so.1")],
            &[],
        )];
        let why = check(
            "app",
            &providers,
            &tree.package(),
            &[],
            "bin/app",
            crate::interp::parse_bundled_interp_rel(b"lib/ld-linux-x86-64.so.2\n"),
        )
        .unwrap_err();
        assert!(why.contains("machine 3"), "{why}");
    }

    #[test]
    fn a_selection_carrying_no_provider_data_passes() {
        let tree = Tree::new("empty");
        check("app", &[], &tree.package(), &[], "bin/app", None).unwrap();
    }
}

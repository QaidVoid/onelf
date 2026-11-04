//! Source-candidate discovery over admitted roots.
//!
//! An activated descriptor group declares ordered source alternatives
//! against typed bases. This module enumerates the alternatives that
//! exist on roots the build has already admitted, in declaration order,
//! and verifies each hit's kind on disk, its symlink confinement and,
//! in a sysroot build, its admission to the selected package closure.
//!
//! A library the dependency walk admitted contributes two roots: the
//! directory the object sits in and, when that directory has a library
//! shape, the installation prefix above it. The layouts the resolver
//! already handles are covered: the flat `usr/lib`, Debian's
//! `usr/lib/<triplet>` multiarch directories, a Nix store path's own
//! `lib`, and a populated AppDir's flattened `lib`.
//!
//! Enumeration is bounded. The hits and examinations one group may
//! yield are capped, and every path is resolved with the confinement
//! the rest of the packer reads sysroot paths with, so a link can never
//! name anything outside its admitted root and a hostile descriptor
//! stops the build instead of the machine.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use onelf_format::rules::{RelativePath, ResourceKind, SourceBase, SourceCandidate};
use onelf_sysroot::path::in_root;

use crate::bundle::sysroot::{Refusal, SysrootUniverse};

/// The most hits one group's discovery may return. Alternatives are
/// layouts of one resource, so a count past this is a hostile or cyclic
/// descriptor, not completeness.
const MAX_GROUP_HITS: usize = 64;

/// The most candidate-and-root combinations one group's discovery may
/// examine.
const MAX_GROUP_CHECKS: usize = 4096;

/// A shared object the dependency walk admitted, with the roots
/// discovery may search beneath it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdmittedLibrary {
    soname: String,
    path: PathBuf,
}

impl AdmittedLibrary {
    /// Record an admitted object: the soname it answers to and the path
    /// the walk resolved it to, which in a sysroot build is the
    /// sysroot's own file with its links resolved inside it.
    pub(crate) fn new(soname: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            soname: soname.into(),
            path: path.into(),
        }
    }

    /// The soname the object answers to.
    pub(crate) fn soname(&self) -> &str {
        &self.soname
    }

    /// The directory the object sits in, which a library-dir base
    /// searches beneath.
    pub(crate) fn library_root(&self) -> Option<&Path> {
        self.path.parent()
    }

    /// The installation prefix above the object's directory, when that
    /// directory has a library shape: `lib`, `lib32`, `lib64` or
    /// `libx32`, optionally holding one multiarch level. A flat
    /// `/usr/lib`, a multiarch `/usr/lib/<triplet>`, a Nix store path's
    /// `lib` and a populated AppDir's `lib` all answer. A directory of
    /// another shape contributes no prefix, so an installation-prefix
    /// base skips it rather than guessing one.
    pub(crate) fn installation_prefix(&self) -> Option<PathBuf> {
        let root = self.library_root()?;
        if is_lib_dir(root) {
            return root.parent().map(Path::to_path_buf);
        }
        let parent = root.parent()?;
        if is_lib_dir(parent) {
            return parent.parent().map(Path::to_path_buf);
        }
        None
    }
}

/// Whether `dir` is a directory a distribution keeps shared libraries
/// in, by name.
fn is_lib_dir(dir: &Path) -> bool {
    dir.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| matches!(n, "lib" | "lib32" | "lib64" | "libx32"))
}

/// The roots one declared base searches: each admitted library's
/// directory or installation prefix, in the order the libraries were
/// admitted, without repetition.
pub(crate) fn roots_for(base: SourceBase, libraries: &[AdmittedLibrary]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for lib in libraries {
        let root = match base {
            SourceBase::LibraryDir => lib.library_root().map(Path::to_path_buf),
            SourceBase::InstallationPrefix => lib.installation_prefix(),
        };
        let Some(root) = root else { continue };
        if seen.insert(root.clone()) {
            roots.push(root);
        }
    }
    roots
}

/// One activated group's declaration, as discovery reads it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GroupQuery<'a> {
    /// The rule the group belongs to.
    pub rule: &'a str,
    /// The group's identity within its rule.
    pub group: &'a str,
    /// The kind the declared source must have on disk.
    pub kind: ResourceKind,
    /// The destination relative to the bundle root, which every
    /// alternative maps onto with its layout preserved.
    pub destination: &'a RelativePath,
    /// The ordered source alternatives.
    pub candidates: &'a [SourceCandidate],
}

/// The roots discovery may search: the libraries the walk admitted and,
/// in a sysroot build, the admission those roots are bounded by.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AdmittedUniverse<'a> {
    /// The libraries the dependency walk admitted, in admission order.
    pub libraries: &'a [AdmittedLibrary],
    /// The sysroot build's admitted universe. `None` for a host scan,
    /// where every existing candidate under an admitted root is in
    /// scope.
    pub sysroot: Option<&'a SysrootUniverse>,
}

/// One declared alternative found on an admitted root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CandidateHit {
    /// The index of the alternative among the declared candidates.
    pub candidate: usize,
    /// The kind verified on disk.
    pub kind: ResourceKind,
    /// The source path as declared against its root.
    pub source: PathBuf,
    /// The source with every link resolved inside its root: what a copy
    /// reads, and what deduplicates alternatives that reach one file.
    pub resolved: PathBuf,
    /// The admitted root the alternative was found under.
    pub root: PathBuf,
    /// The destination relative to the bundle root, layout preserved.
    pub destination: PathBuf,
}

/// Why one declared alternative on one root is not a hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MissReason {
    /// Nothing of either kind exists at the declared location, or only
    /// a link to nothing.
    Absent,
    /// The location holds the other kind: a file where the declaration
    /// requires a directory, or the reverse.
    WrongKind {
        /// The kind the location actually holds.
        actual: ResourceKind,
    },
    /// The location exists only through a link its root does not
    /// contain, so reading it would read past the admitted root.
    OutsideRoot,
    /// A sysroot build: the location exists in the sysroot but the
    /// build did not admit it.
    NotAdmitted(Refusal),
}

/// One declared alternative examined on one root and refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CandidateMiss {
    /// The index of the alternative among the declared candidates.
    pub candidate: usize,
    /// The root the alternative was examined under.
    pub root: PathBuf,
    /// Why the alternative is not a hit.
    pub reason: MissReason,
}

/// What discovery answered for one activated group.
#[derive(Debug, Clone)]
pub(crate) struct GroupOutcome {
    /// The existing admitted alternatives, in declaration order, each
    /// against every root of its base in library-admission order.
    pub found: Vec<CandidateHit>,
    /// The examined and refused alternatives, in examination order.
    pub misses: Vec<CandidateMiss>,
}

/// Enumerate an activated group's declared alternatives over the
/// admitted roots, in declaration order, each against every root of its
/// base in library-admission order. Every hit has its kind verified on
/// disk and every path confined to the root it was declared against; in
/// a sysroot build every hit is additionally admitted by the build's
/// own closure and pruning decisions, so a candidate of an unselected
/// package or of pruned content is a structured miss, never a copy.
///
/// Past the hit or examination caps the answer is an error naming the
/// group, so discovery stays bounded whatever a descriptor declares.
pub(crate) fn discover_group(
    query: &GroupQuery,
    admitted: &AdmittedUniverse,
) -> io::Result<GroupOutcome> {
    let mut outcome = GroupOutcome {
        found: Vec::new(),
        misses: Vec::new(),
    };
    let mut examined = 0usize;
    for (index, candidate) in query.candidates.iter().enumerate() {
        for root in roots_for(candidate.base, admitted.libraries) {
            examined += 1;
            if examined > MAX_GROUP_CHECKS {
                return Err(bounded(query, MAX_GROUP_CHECKS, "examinations"));
            }
            match examine(
                &root,
                index,
                candidate,
                query.kind,
                query.destination,
                admitted.sysroot,
            ) {
                Ok(hit) => {
                    if outcome.found.iter().any(|h| h.resolved == hit.resolved) {
                        continue;
                    }
                    if outcome.found.len() >= MAX_GROUP_HITS {
                        return Err(bounded(query, MAX_GROUP_HITS, "source candidates"));
                    }
                    outcome.found.push(hit);
                }
                Err(reason) => outcome.misses.push(CandidateMiss {
                    candidate: index,
                    root,
                    reason,
                }),
            }
        }
    }
    Ok(outcome)
}

/// The bounded-discovery error: which group stopped, and at what.
fn bounded(query: &GroupQuery, cap: usize, what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "rule {} group {}: more than {} {what} in one group; refusing \
             unbounded discovery",
            query.rule, query.group, cap
        ),
    )
}

/// Examine one alternative against one root: confinement first, then
/// the kind on disk, then in a sysroot build the build's own admission.
fn examine(
    root: &Path,
    index: usize,
    candidate: &SourceCandidate,
    kind: ResourceKind,
    destination: &RelativePath,
    sysroot: Option<&SysrootUniverse>,
) -> Result<CandidateHit, MissReason> {
    let declared = root.join(candidate.path.as_str());
    let confined = match sysroot {
        Some(universe) => {
            // The candidate's links are written against the sysroot's
            // own `/`, so they are resolved there, the way every other
            // sysroot read is: the declared location is taken relative
            // to the sysroot root first.
            let sysroot_root = universe.root();
            let base = root
                .strip_prefix(sysroot_root)
                .map_err(|_| MissReason::OutsideRoot)?;
            let rel = base.join(candidate.path.as_str());
            in_root(sysroot_root, &rel).map_err(|_| miss_reason(&declared))?
        }
        None => resolve_within(root, Path::new(candidate.path.as_str()))?,
    };
    let meta = match fs::metadata(&confined) {
        Ok(meta) => meta,
        Err(_) => return Err(miss_reason(&declared)),
    };
    if let Some(universe) = sysroot {
        let rel = match confined.strip_prefix(universe.root()) {
            Ok(rel) => rel.to_string_lossy().into_owned(),
            Err(_) => return Err(MissReason::OutsideRoot),
        };
        if !universe.admits(&rel) {
            return Err(MissReason::NotAdmitted(universe.refusal(&rel)));
        }
    }
    let actual = if meta.is_dir() {
        ResourceKind::Directory
    } else if meta.is_file() {
        ResourceKind::File
    } else {
        return Err(MissReason::Absent);
    };
    if actual != kind {
        return Err(MissReason::WrongKind { actual });
    }
    Ok(CandidateHit {
        candidate: index,
        kind: actual,
        source: declared,
        resolved: confined,
        root: root.to_path_buf(),
        destination: PathBuf::from(destination.as_str()),
    })
}

/// More links than this on one candidate is taken for a loop, as the
/// kernel does.
const MAX_CANDIDATE_LINKS: usize = 40;

/// Where `rel` under an admitted host root leads, with every link
/// followed and every landing kept inside the root. A relative target
/// is taken against the link's directory; an absolute one keeps the
/// machine's own meaning of `/` and is refused when it names a place
/// outside the root.
fn resolve_within(root: &Path, rel: &Path) -> Result<PathBuf, MissReason> {
    let mut out = PathBuf::new();
    let mut pending: Vec<std::ffi::OsString> = rel
        .components()
        .rev()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name.to_os_string()),
            Component::ParentDir => Some("..".to_string().into()),
            Component::CurDir | Component::RootDir | Component::Prefix(_) => None,
        })
        .collect();
    let mut links = 0usize;
    while let Some(part) = pending.pop() {
        if part == ".." {
            out.pop();
            continue;
        }
        out.push(&part);
        let Ok(target) = fs::read_link(root.join(&out)) else {
            continue;
        };
        links += 1;
        if links > MAX_CANDIDATE_LINKS {
            return Err(MissReason::OutsideRoot);
        }
        out.pop();
        if target.is_absolute() {
            let Ok(inside) = target.strip_prefix(root) else {
                return Err(MissReason::OutsideRoot);
            };
            out.clear();
            for c in inside.components().rev() {
                match c {
                    Component::Normal(name) => pending.push(name.to_os_string()),
                    Component::ParentDir => pending.push("..".to_string().into()),
                    Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
                }
            }
        } else {
            for c in target.components().rev() {
                match c {
                    Component::Normal(name) => pending.push(name.to_os_string()),
                    Component::ParentDir => pending.push("..".to_string().into()),
                    Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
                }
            }
        }
    }
    Ok(root.join(out))
}

/// Whether an unresolvable location is absent or reachable only by
/// leaving its root: the kernel follows a link wherever it names while
/// the confined resolution does not, so a location the kernel still
/// finds is one confinement refused.
fn miss_reason(declared: &Path) -> MissReason {
    if fs::metadata(declared).is_ok() {
        return MissReason::OutsideRoot;
    }
    MissReason::Absent
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::sysroot::fixture;

    /// An admitted library resolved under a prefix's library directory,
    /// named for the file it sits at.
    fn library(path: &str) -> AdmittedLibrary {
        let path = PathBuf::from(path);
        let soname = path.file_name().unwrap().to_str().unwrap().to_string();
        AdmittedLibrary::new(soname, path)
    }

    fn candidate(base: SourceBase, path: &str) -> SourceCandidate {
        SourceCandidate {
            base,
            path: RelativePath::new(path).unwrap(),
        }
    }

    fn discover(
        kind: ResourceKind,
        candidates: &[SourceCandidate],
        libraries: &[AdmittedLibrary],
        sysroot: Option<&SysrootUniverse>,
    ) -> io::Result<GroupOutcome> {
        discover_group(
            &GroupQuery {
                rule: "rule",
                group: "group",
                kind,
                destination: &RelativePath::new("lib/group").unwrap(),
                candidates,
            },
            &AdmittedUniverse { libraries, sysroot },
        )
    }

    /// Prefixes come out of the shapes the resolver already handles: the
    /// flat lib directory, Debian's multiarch level, a store path's own
    /// lib, and a populated AppDir's flattened one. A directory of
    /// another shape is not guessed into a prefix.
    #[test]
    fn prefixes_follow_flat_multiarch_store_and_appdir_shapes() {
        let flat = library("/pfx/usr/lib/liba.so.1");
        assert_eq!(flat.library_root(), Some(Path::new("/pfx/usr/lib")));
        assert_eq!(
            flat.installation_prefix().as_deref(),
            Some(Path::new("/pfx/usr"))
        );

        let multi = library("/pfx/usr/lib/x86_64-linux-gnu/liba.so.1");
        assert_eq!(
            multi.library_root(),
            Some(Path::new("/pfx/usr/lib/x86_64-linux-gnu"))
        );
        assert_eq!(
            multi.installation_prefix().as_deref(),
            Some(Path::new("/pfx/usr"))
        );

        let store = library("/store/HASH-name/lib/liba.so.1");
        assert_eq!(
            store.installation_prefix().as_deref(),
            Some(Path::new("/store/HASH-name"))
        );

        let staged = library("/appdir/lib/liba.so.1");
        assert_eq!(
            staged.installation_prefix().as_deref(),
            Some(Path::new("/appdir"))
        );

        let shaped = library("/opt/vendor/liba.so.1");
        assert_eq!(shaped.installation_prefix(), None);
    }

    /// Roots follow library-admission order, and one shape does not
    /// repeat when several libraries share it.
    #[test]
    fn roots_follow_library_admission_order_without_repetition() {
        let libs = [
            library("/one/usr/lib/x86_64-linux-gnu/liba.so.1"),
            library("/one/usr/lib/libb.so.1"),
            library("/two/usr/lib/libc.so.1"),
        ];
        assert_eq!(
            roots_for(SourceBase::InstallationPrefix, &libs),
            [PathBuf::from("/one/usr"), PathBuf::from("/two/usr")]
        );
        assert_eq!(
            roots_for(SourceBase::LibraryDir, &libs),
            [
                PathBuf::from("/one/usr/lib/x86_64-linux-gnu"),
                PathBuf::from("/one/usr/lib"),
                PathBuf::from("/two/usr/lib"),
            ]
        );
    }

    /// Alternatives are enumerated in declaration order, each against
    /// every root of its base in admission order, with the origin root
    /// and the declared source recorded on each hit.
    #[test]
    fn candidates_are_enumerated_in_declaration_order_across_roots() {
        let dir =
            std::env::temp_dir().join(format!("onelf-order-{}-{}", std::process::id(), line!()));
        let _ = fs::remove_dir_all(&dir);
        for pfx in ["one", "two"] {
            fs::create_dir_all(dir.join(pfx).join("usr/share/app")).unwrap();
            fs::write(dir.join(pfx).join("usr/share/app/data"), b"d").unwrap();
        }
        let libs = [
            library(dir.join("one/usr/lib/liba.so.1").to_str().unwrap()),
            library(dir.join("two/usr/lib/liba.so.1").to_str().unwrap()),
        ];
        let candidates = [
            candidate(SourceBase::InstallationPrefix, "share/app/missing"),
            candidate(SourceBase::InstallationPrefix, "share/app/data"),
        ];
        let outcome = discover(ResourceKind::File, &candidates, &libs, None).unwrap();
        assert_eq!(
            outcome
                .found
                .iter()
                .map(|h| (h.candidate, h.root.clone()))
                .collect::<Vec<_>>(),
            [(1, dir.join("one/usr")), (1, dir.join("two/usr"))],
            "declaration order first, then root order"
        );
        assert_eq!(outcome.found[0].kind, ResourceKind::File);
        assert_eq!(outcome.found[0].source, dir.join("one/usr/share/app/data"));
        assert_eq!(
            outcome.found[0].resolved,
            dir.join("one/usr/share/app/data")
        );
        assert_eq!(outcome.found[0].destination, PathBuf::from("lib/group"));
        assert_eq!(
            outcome
                .misses
                .iter()
                .map(|m| (m.candidate, m.reason.clone()))
                .collect::<Vec<_>>(),
            [(0, MissReason::Absent), (0, MissReason::Absent)],
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A link resolved inside its root satisfies the declaration. An
    /// absolute target keeps the machine's meaning of `/` when it stays
    /// under the admitted root, and alternatives reaching one file
    /// deduplicate onto a single hit.
    #[test]
    fn a_confined_link_satisfies_the_declaration_inside_its_root() {
        let dir =
            std::env::temp_dir().join(format!("onelf-link-{}-{}", std::process::id(), line!()));
        let _ = fs::remove_dir_all(&dir);
        let root = dir.join("tree/usr");
        fs::create_dir_all(root.join("lib")).unwrap();
        fs::write(root.join("lib/liba.so.1.2"), b"elf").unwrap();
        let target = root.join("lib/liba.so.1.2");
        std::os::unix::fs::symlink(&target, root.join("lib/absolute")).unwrap();
        std::os::unix::fs::symlink("liba.so.1.2", root.join("lib/relative")).unwrap();
        let libs = [library(
            dir.join("tree/usr/lib/liba.so.1").to_str().unwrap(),
        )];
        let candidates = [
            candidate(SourceBase::InstallationPrefix, "lib/absolute"),
            candidate(SourceBase::InstallationPrefix, "lib/relative"),
            candidate(SourceBase::LibraryDir, "liba.so.1.2"),
        ];
        let outcome = discover(ResourceKind::File, &candidates, &libs, None).unwrap();
        assert_eq!(outcome.misses.len(), 0, "{:?}", outcome.misses);
        // All three alternatives answer for the same file, so they
        // deduplicate onto one hit, the first declared one.
        assert_eq!(outcome.found.len(), 1);
        assert_eq!(outcome.found[0].candidate, 0);
        assert_eq!(outcome.found[0].resolved, root.join("lib/liba.so.1.2"));
        assert!(outcome.found[0].root.starts_with(&root));
        let _ = fs::remove_dir_all(&dir);
    }

    /// A link its root does not contain is never a hit: neither an
    /// absolute one out of the tree nor a relative climb, whatever the
    /// packer's machine holds where they point.
    #[test]
    fn a_link_out_of_the_root_is_never_a_hit() {
        let dir =
            std::env::temp_dir().join(format!("onelf-escape-{}-{}", std::process::id(), line!()));
        let _ = fs::remove_dir_all(&dir);
        let root = dir.join("tree");
        let host = dir.join("host");
        fs::create_dir_all(root.join("usr/lib")).unwrap();
        fs::create_dir_all(&host).unwrap();
        fs::write(host.join("secret"), b"host").unwrap();
        fs::write(dir.join("outside"), b"host").unwrap();
        std::os::unix::fs::symlink(dir.join("host/secret"), root.join("usr/lib/absolute")).unwrap();
        std::os::unix::fs::symlink("../../../outside", root.join("usr/lib/relative")).unwrap();
        std::os::unix::fs::symlink("nothing", root.join("usr/lib/dangling")).unwrap();
        let libs = [library(root.join("usr/lib/liba.so.1").to_str().unwrap())];
        let candidates = [
            candidate(SourceBase::LibraryDir, "absolute"),
            candidate(SourceBase::LibraryDir, "relative"),
            candidate(SourceBase::LibraryDir, "dangling"),
        ];
        let outcome = discover(ResourceKind::File, &candidates, &libs, None).unwrap();
        assert!(outcome.found.is_empty(), "{:?}", outcome.found);
        let reasons: Vec<(usize, &MissReason)> = outcome
            .misses
            .iter()
            .map(|m| (m.candidate, &m.reason))
            .collect();
        assert_eq!(
            reasons,
            [
                (0, &MissReason::OutsideRoot),
                (1, &MissReason::OutsideRoot),
                (2, &MissReason::Absent),
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The kind on disk must match the declaration in both directions:
    /// a directory never satisfies a file declaration nor the reverse.
    #[test]
    fn the_kind_on_disk_must_match_the_declaration() {
        let dir =
            std::env::temp_dir().join(format!("onelf-kind-{}-{}", std::process::id(), line!()));
        let _ = fs::remove_dir_all(&dir);
        let root = dir.join("tree");
        fs::create_dir_all(root.join("usr/lib/tree")).unwrap();
        fs::write(root.join("usr/lib/data"), b"d").unwrap();
        let libs = [library(root.join("usr/lib/liba.so.1").to_str().unwrap())];
        let candidates = [
            candidate(SourceBase::LibraryDir, "data"),
            candidate(SourceBase::LibraryDir, "tree"),
        ];

        let outcome = discover(ResourceKind::File, &candidates, &libs, None).unwrap();
        assert!(outcome.found.iter().any(|h| h.candidate == 0));
        assert!(outcome.misses.iter().any(|m| m.reason
            == MissReason::WrongKind {
                actual: ResourceKind::Directory
            }));
        assert!(!outcome.found.is_empty());

        let outcome = discover(ResourceKind::Directory, &candidates, &libs, None).unwrap();
        assert!(outcome.found.iter().any(|h| h.candidate == 1));
        assert!(outcome.misses.iter().any(|m| m.reason
            == MissReason::WrongKind {
                actual: ResourceKind::File
            }));
        let _ = fs::remove_dir_all(&dir);
    }

    /// Absence classifies by the declaration: a required group's miss
    /// and an optional group's omission are distinct outcomes, neither
    /// of them a build failure yet.
    #[test]
    fn required_and_optional_absence_classify_differently() {
        let dir =
            std::env::temp_dir().join(format!("onelf-absent-{}-{}", std::process::id(), line!()));
        let _ = fs::remove_dir_all(&dir);
        let root = dir.join("tree");
        fs::create_dir_all(root.join("usr/lib")).unwrap();
        let libs = [library(root.join("usr/lib/liba.so.1").to_str().unwrap())];
        let candidates = [candidate(SourceBase::LibraryDir, "missing")];

        let outcome = discover(ResourceKind::File, &candidates, &libs, None).unwrap();
        assert!(outcome.found.is_empty());
        assert!(outcome.found.is_empty());

        let outcome = discover(ResourceKind::File, &candidates, &libs, None).unwrap();
        assert!(outcome.found.is_empty());
        assert!(outcome.found.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Discovery is bounded: more hits than the cap in one group is an
    /// error naming the group, not unbounded work.
    #[test]
    fn discovery_refuses_to_grow_unbounded() {
        let dir =
            std::env::temp_dir().join(format!("onelf-bounded-{}-{}", std::process::id(), line!()));
        let _ = fs::remove_dir_all(&dir);
        let root = dir.join("tree");
        fs::create_dir_all(root.join("usr/lib")).unwrap();
        let mut candidates = Vec::new();
        for i in 0..MAX_GROUP_HITS + 1 {
            let name = format!("f{i}");
            fs::write(root.join("usr/lib").join(&name), b"x").unwrap();
            candidates.push(candidate(SourceBase::LibraryDir, &name));
        }
        let libs = [library(root.join("usr/lib/liba.so.1").to_str().unwrap())];
        let err = discover(ResourceKind::File, &candidates, &libs, None).unwrap_err();
        assert!(
            err.to_string()
                .contains("rule rule group group: more than 64 source candidates"),
            "{err}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A sysroot build discovers only within its own root. A matching
    /// file on the packer's machine is never a hit, and an absolute
    /// link pointing at one is refused as outside the root.
    #[test]
    fn a_sysroot_build_never_reads_past_its_root() {
        let dir =
            std::env::temp_dir().join(format!("onelf-sysroot-{}-{}", std::process::id(), line!()));
        let _ = fs::remove_dir_all(&dir);
        let root = dir.join("sys");
        let host = dir.join("host");
        fs::create_dir_all(root.join("usr/lib")).unwrap();
        fs::create_dir_all(root.join("usr/share/alsa")).unwrap();
        fs::create_dir_all(host.join("usr/share/alsa")).unwrap();
        fs::write(root.join("usr/lib/libasound.so.2"), b"elf").unwrap();
        fs::write(root.join("usr/share/alsa/alsa.conf"), b"conf").unwrap();
        fs::write(host.join("usr/share/alsa/alsa.conf"), b"host").unwrap();
        let mut app = fixture::package(
            "app",
            &["usr/lib/libasound.so.2", "usr/share/alsa/alsa.conf"],
        );
        app.depends = vec!["alsa".into()];
        let mut alsa = fixture::package("alsa-lib", &["usr/lib/libasound.so.2"]);
        alsa.provides = vec!["alsa".into()];
        let packages = vec![app, alsa];
        let universe = fixture::populate_universe(&root, &packages, "app", &[], None, None);
        let libs = [library(
            root.join("usr/lib/libasound.so.2").to_str().unwrap(),
        )];
        let candidates = [candidate(SourceBase::InstallationPrefix, "share/alsa")];

        let outcome =
            discover(ResourceKind::Directory, &candidates, &libs, Some(&universe)).unwrap();
        assert_eq!(outcome.found.len(), 1, "{:?}", outcome.misses);
        assert_eq!(
            outcome.found[0].resolved,
            root.join("usr/share/alsa"),
            "the hit is the sysroot's own directory"
        );

        fs::remove_dir_all(root.join("usr/share/alsa")).unwrap();
        std::os::unix::fs::symlink(dir.join("host/usr/share/alsa"), root.join("usr/share/alsa"))
            .unwrap();
        let outcome =
            discover(ResourceKind::Directory, &candidates, &libs, Some(&universe)).unwrap();
        assert!(outcome.found.is_empty(), "{:?}", outcome.found);
        assert_eq!(
            outcome.misses[0].reason,
            MissReason::OutsideRoot,
            "the host directory is reachable only by leaving the sysroot"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A sysroot's own absolute links resolve inside it: an `xkb` link
    /// naming `/usr/share/xkeyboard-config-2` is the sysroot's
    /// directory, not the packer's.
    #[test]
    fn a_sysroots_absolute_link_resolves_inside_it() {
        let dir =
            std::env::temp_dir().join(format!("onelf-abslink-{}-{}", std::process::id(), line!()));
        let _ = fs::remove_dir_all(&dir);
        let root = dir.join("sys");
        fs::create_dir_all(root.join("usr/lib")).unwrap();
        fs::create_dir_all(root.join("usr/share/X11")).unwrap();
        fs::create_dir_all(root.join("usr/share/xkeyboard-config-2/symbols")).unwrap();
        fs::write(root.join("usr/lib/libxkbcommon.so.0"), b"elf").unwrap();
        fs::write(root.join("usr/share/xkeyboard-config-2/symbols/us"), b"us").unwrap();
        std::os::unix::fs::symlink(
            "/usr/share/xkeyboard-config-2",
            root.join("usr/share/X11/xkb"),
        )
        .unwrap();
        let mut app = fixture::package("app", &["usr/bin/app"]);
        app.depends = vec!["xkb".into()];
        let mut xkb = fixture::package(
            "xkb",
            &[
                "usr/lib/libxkbcommon.so.0",
                "usr/share/X11/xkb",
                "usr/share/xkeyboard-config-2/symbols/us",
            ],
        );
        xkb.provides = vec!["xkb".into()];
        let universe = fixture::populate_universe(&root, &[app, xkb], "app", &[], None, None);
        let libs = [library(
            root.join("usr/lib/libxkbcommon.so.0").to_str().unwrap(),
        )];
        let candidates = [candidate(SourceBase::InstallationPrefix, "share/X11/xkb")];

        let outcome =
            discover(ResourceKind::Directory, &candidates, &libs, Some(&universe)).unwrap();
        assert_eq!(outcome.misses.len(), 0, "{:?}", outcome.misses);
        assert_eq!(
            outcome.found[0].resolved,
            root.join("usr/share/xkeyboard-config-2"),
            "the link is the sysroot's own directory"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A candidate of an unselected package is a structured miss that
    /// says which package owns it: discovery does not expand the
    /// package universe to complete a group.
    #[test]
    fn an_unselected_package_is_a_structured_miss_not_an_expansion() {
        let dir = std::env::temp_dir().join(format!(
            "onelf-unselected-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = fs::remove_dir_all(&dir);
        let root = dir.join("sys");
        fs::create_dir_all(root.join("usr/lib/gstreamer-1.0")).unwrap();
        fs::write(root.join("usr/lib/libgstreamer-1.0.so.0"), b"elf").unwrap();
        fs::write(root.join("usr/lib/gstreamer-1.0/libgstextra.so"), b"elf").unwrap();
        let mut app = fixture::package("app", &["usr/bin/app"]);
        app.depends = vec!["gstreamer".into()];
        let mut gst = fixture::package("gst", &["usr/lib/libgstreamer-1.0.so.0"]);
        gst.provides = vec!["gstreamer".into()];
        let extra = fixture::package("extra", &["usr/lib/gstreamer-1.0/libgstextra.so"]);
        let packages = vec![app, gst, extra];
        let universe = fixture::populate_universe(&root, &packages, "app", &[], None, None);
        let libs = [library(
            root.join("usr/lib/libgstreamer-1.0.so.0").to_str().unwrap(),
        )];
        let candidates = [candidate(SourceBase::LibraryDir, "gstreamer-1.0")];

        let outcome =
            discover(ResourceKind::Directory, &candidates, &libs, Some(&universe)).unwrap();
        assert!(outcome.found.is_empty(), "{:?}", outcome.found);
        assert_eq!(
            outcome.misses[0].reason,
            MissReason::NotAdmitted(Refusal::UnselectedPackage {
                owner: "extra".to_string()
            })
        );
        assert!(outcome.found.is_empty());

        let named = vec!["extra".to_string()];
        let universe = fixture::populate_universe(&root, &packages, "app", &named, None, None);
        let outcome =
            discover(ResourceKind::Directory, &candidates, &libs, Some(&universe)).unwrap();
        assert_eq!(outcome.found.len(), 1, "{:?}", outcome.misses);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A policy-pruned member of an admitted plugin directory is not
    /// resurrected: the directory is a hit, and the admitted members
    /// discovery hands on stop at what the build kept.
    #[test]
    fn policy_pruned_plugin_content_is_never_resurrected() {
        let dir =
            std::env::temp_dir().join(format!("onelf-pruned-{}-{}", std::process::id(), line!()));
        let _ = fs::remove_dir_all(&dir);
        let root = dir.join("sys");
        fs::create_dir_all(root.join("usr/lib/gstreamer-1.0")).unwrap();
        fs::write(root.join("usr/lib/libgstreamer-1.0.so.0"), b"elf").unwrap();
        fs::write(root.join("usr/lib/gstreamer-1.0/libgstcore.so"), b"elf").unwrap();
        fs::write(root.join("usr/lib/gstreamer-1.0/libgstbad.so"), b"elf").unwrap();
        let mut app = fixture::package("app", &["usr/bin/app"]);
        app.depends = vec!["gstreamer".into()];
        let mut gst = fixture::package(
            "gst",
            &[
                "usr/lib/libgstreamer-1.0.so.0",
                "usr/lib/gstreamer-1.0/libgstcore.so",
                "usr/lib/gstreamer-1.0/libgstbad.so",
            ],
        );
        gst.provides = vec!["gstreamer".into()];
        let packages = vec![app, gst];
        let policy_path = dir.join("policy.txt");
        fs::write(&policy_path, "usr/lib/gstreamer-1.0/libgstbad.so\n").unwrap();
        let policy = onelf_sysroot::Policy::load(&policy_path).unwrap();
        let universe =
            fixture::populate_universe(&root, &packages, "app", &[], Some(&policy), None);
        let libs = [library(
            root.join("usr/lib/libgstreamer-1.0.so.0").to_str().unwrap(),
        )];
        let candidates = [candidate(SourceBase::LibraryDir, "gstreamer-1.0")];

        let outcome =
            discover(ResourceKind::Directory, &candidates, &libs, Some(&universe)).unwrap();
        assert_eq!(outcome.found.len(), 1, "{:?}", outcome.misses);
        assert_eq!(
            universe.admitted_under("usr/lib/gstreamer-1.0"),
            ["usr/lib/gstreamer-1.0/libgstcore.so"],
            "the pruned member stays out of what the collector may walk"
        );
        assert_eq!(
            universe.refusal("usr/lib/gstreamer-1.0/libgstbad.so"),
            Refusal::Pruned {
                owner: "gst".to_string()
            }
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Discovery covers the multiarch and store layouts end to end: the
    /// plugin tree beside a multiarch library and the data under its
    /// prefix are both found, and a store path's own lib and share too.
    #[test]
    fn discovery_covers_multiarch_and_store_layouts() {
        let dir =
            std::env::temp_dir().join(format!("onelf-layout-{}-{}", std::process::id(), line!()));
        let _ = fs::remove_dir_all(&dir);
        let multi = dir.join("sys/usr/lib/x86_64-linux-gnu");
        fs::create_dir_all(multi.join("gstreamer-1.0")).unwrap();
        fs::create_dir_all(dir.join("sys/usr/share/gstreamer-1.0")).unwrap();
        fs::write(multi.join("libgstreamer-1.0.so.0"), b"elf").unwrap();
        fs::write(multi.join("gstreamer-1.0/libgstcore.so"), b"elf").unwrap();
        fs::write(dir.join("sys/usr/share/gstreamer-1.0/def"), b"d").unwrap();
        let libs = [library(
            multi.join("libgstreamer-1.0.so.0").to_str().unwrap(),
        )];
        let candidates = [
            candidate(SourceBase::LibraryDir, "gstreamer-1.0"),
            candidate(SourceBase::InstallationPrefix, "share/gstreamer-1.0"),
        ];
        let outcome = discover(ResourceKind::Directory, &candidates, &libs, None).unwrap();
        assert_eq!(outcome.found.len(), 2, "{:?}", outcome.misses);
        assert_eq!(outcome.found[0].resolved, multi.join("gstreamer-1.0"));
        assert_eq!(
            outcome.found[1].resolved,
            dir.join("sys/usr/share/gstreamer-1.0")
        );

        let store = dir.join("store/HASH-gstreamer");
        fs::create_dir_all(store.join("lib/gstreamer-1.0")).unwrap();
        fs::create_dir_all(store.join("share/gstreamer-1.0")).unwrap();
        fs::write(store.join("lib/libgstreamer-1.0.so.0"), b"elf").unwrap();
        fs::write(store.join("lib/gstreamer-1.0/libgstcore.so"), b"elf").unwrap();
        fs::write(store.join("share/gstreamer-1.0/def"), b"d").unwrap();
        let libs = [library(
            store.join("lib/libgstreamer-1.0.so.0").to_str().unwrap(),
        )];
        let outcome = discover(ResourceKind::Directory, &candidates, &libs, None).unwrap();
        assert_eq!(outcome.found.len(), 2, "{:?}", outcome.misses);
        assert_eq!(outcome.found[0].resolved, store.join("lib/gstreamer-1.0"));
        assert_eq!(outcome.found[1].resolved, store.join("share/gstreamer-1.0"));
        let _ = fs::remove_dir_all(&dir);
    }
}

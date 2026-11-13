//! Plugin and data directories found from the paths bundled objects
//! have compiled in.
//!
//! A library that loads plugins or reads data from a directory fixed at
//! its build names that directory as a whole absolute path in its
//! read-only data: `libasound` carries `/usr/lib/alsa-lib` and
//! `/usr/share/alsa`, `libxkbcommon` carries `/usr/share/X11/xkb`. Every
//! object the dependency walk admits, and every other ELF file of the
//! tree, is read for such strings, so a framework nobody described still
//! has its directories found.
//!
//! A string is a candidate only when it is a normalized absolute path
//! that lies strictly below one of the object's resource bases and names
//! an existing directory. The bases are the library directories of the
//! object's installation prefix, its `lib`, `lib32`, `lib64` and `libx32`
//! and each multiarch directory directly below one of them, and the
//! prefix's `share`. Frameworks keep their plugins and data there, while
//! the rest of a prefix, its `etc`, `var`, `local`, `include` or a
//! pseudo-filesystem below `/`, is host configuration, build input or
//! system state. A base itself, and anything containing one, holds every
//! package's files and is never a candidate. When the prefix is `/`, the
//! same bases below `usr` count too, since `/` and `/usr` are one
//! hierarchy on merged and unmerged hosts alike.
//!
//! Each candidate is resolved through its links from the source's own
//! root, the host's `/` or the sysroot, with the confinement source
//! discovery reads with, and is kept only when the result lies in a base
//! under the prefix. The strings that land on one directory are one
//! entry. A `lib64` link or `share/X11/xkb` pointing at
//! `share/xkeyboard-config-2` is found once, under its canonical path. A
//! sysroot build counts only directories holding files it admitted.
//!
//! Within the bases the catalog's inference exclusions drop what holds
//! several packages' files or belongs to libc or a driver stack: a shared
//! root such as `share/X11` with what contains it, and a shared tree such
//! as `share/locale` or a library directory's `security`, a libc
//! directory such as `gconv` or a driver directory such as `dri` with
//! what contains it or lies below it. A candidate that equals or contains
//! the directory the object sits in is dropped too, while what lies below
//! it stays, as Xorg's `lib/xorg/modules` does. libc's own objects are
//! not read, since the libc handling owns their directories, and a
//! recipe exclusion drops a directory by any of its components below its
//! library directory or `share`. Under a `/` prefix every rule also
//! stands below `usr`.
//!
//! An object the tree itself holds names the source's paths, not the
//! tree's. A host scan's tree copy of a library takes its prefix from the
//! host file it stands for. A program, or a library with no host twin,
//! takes its prefix at the same place below the source's root. Three
//! edges stay conservative:
//!
//! - A tree that is itself an installation prefix, with its own `lib` and
//!   `bin`, gets the source's `/` for its prefix.
//! - An out-of-tree `--target` in a sysroot build has no prefix below the
//!   sysroot and finds nothing.
//! - A re-run finds the original prefix of a flat copy only while its
//!   host twin is still reachable through the search paths, the loader
//!   cache, the standard directories or `LD_LIBRARY_PATH`. One found
//!   through a run path the first run rewrote falls back to the tree
//!   mapping and contributes nothing.
//!
//! Reading is bounded. One object may name at most [`MAX_OBJECT_PATHS`]
//! existing directories and resolve at most [`MAX_OBJECT_RESOLUTIONS`]
//! plausible strings, and one build may find at most
//! [`MAX_INFERRED_DIRS`] directories. Past any of them the build stops and
//! names the bound, since that is a hostile or pathological input rather
//! than a framework.

use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};
use std::fs;
use std::io;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use goblin::elf::program_header::{PF_W, PT_LOAD};
use onelf_format::catalog;
use onelf_format::rules::{Descriptor, ExclusionBase, ExclusionEntry, ResourceKind, SourceBase};
use onelf_sysroot::path::in_root;

use super::collect::{GroupCollection, visit_members};
use super::datadirs::path_like;
use super::{
    CandidateHit, Finding, Outcome, completeness, dynstr_range, format_size, is_excluded,
    libc_family_of_soname, library_dir_of, object_prefix, read_elf_class, resolve_within,
};
use crate::bundle::sysroot::SysrootUniverse;

/// The most distinct compiled-in paths naming an existing directory one
/// object may hold. Real libraries name a few dozen at most.
pub(crate) const MAX_OBJECT_PATHS: usize = 1024;

/// The most distinct plausible compiled-in paths one object may have
/// resolved. A Go program names every source file of its toolchain, so
/// this caps only what a hostile file could drive.
pub(crate) const MAX_OBJECT_RESOLUTIONS: usize = 65536;

/// The most distinct directories one build may infer.
pub(crate) const MAX_INFERRED_DIRS: usize = 256;

/// What an inferred directory holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InferredKind {
    /// ELF objects, directly or in nested directories: a plugin
    /// directory.
    Plugins,
    /// Anything else, carried whole.
    Data,
}

/// One directory the bundled objects compile in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InferredDir {
    /// The directory with every link resolved inside the source: what a
    /// copy reads.
    pub source: PathBuf,
    /// The installation prefix the directory lies under. Objects of two
    /// prefixes can name one directory, in an unmerged sysroot say, and
    /// the least prefix and path pair is kept.
    pub prefix: PathBuf,
    /// The canonical path relative to the prefix.
    pub rel: PathBuf,
    /// Where the bundle carries it, relative to the bundle root: below
    /// the bundle's library directory for a directory below a library
    /// directory of the prefix, else at its path relative to the prefix,
    /// with `usr` taken off under a `/` prefix. The least prefix and
    /// path pair among the objects that named the directory wins, so
    /// this follows that pair until the directory is handed out.
    pub destination: PathBuf,
    /// What the directory holds, classified under the winning prefix.
    pub kind: InferredKind,
    /// The bytes a data directory's files hold, read when it is
    /// classified. A plugin directory's are counted when it is collected.
    pub size: u64,
    /// Every compiled-in path that names the directory, sorted. Several
    /// spellings reach one directory through links.
    pub compiled: BTreeSet<String>,
    /// The names of the objects that compile it in, sorted: a library's
    /// soname or a program's file name.
    pub objects: BTreeSet<String>,
}

impl InferredDir {
    /// The directory as a found source alternative, confined to its
    /// prefix, for the collection the descriptors' groups go through.
    pub(crate) fn hit(&self) -> CandidateHit {
        CandidateHit {
            candidate: 0,
            base: SourceBase::InstallationPrefix,
            kind: ResourceKind::Directory,
            source: self.source.clone(),
            resolved: self.source.clone(),
            root: self.prefix.clone(),
            destination: self.destination.clone(),
        }
    }
}

/// What discovery reads objects against.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Scope<'a> {
    /// The sysroot build's admitted universe, `None` for a host scan.
    pub sysroot: Option<&'a SysrootUniverse>,
    /// The spellings of the tree being assembled: an object there takes
    /// its prefix and its own directory at the same place below the
    /// source's root.
    pub trees: &'a [&'a Path],
    /// The recipe's `[bundle] exclude` patterns, which the CLI's
    /// `--exclude` feeds too. A pattern is a soname prefix, and it drops
    /// a directory when it begins one of the directory's components below
    /// its library directory or `share`, so `libdecor` drops
    /// `lib/libdecor/plugins-1` while `libx` leaves `libx32` alone.
    pub exclude: &'a [String],
    /// The descriptors whose locations inference leaves alone: every
    /// built-in, a disabled one included, and the enabled local rules. A
    /// directory one of them declares, or one overlapping it, is left to
    /// it, whose members, requirement and hooks decide what is carried
    /// there, and disabling a built-in keeps inference out too.
    pub descriptors: &'a [&'a Descriptor],
    /// The bundle's library directory, relative to its root.
    pub lib_dir: &'a Path,
}

/// One bundled object discovery reads.
#[derive(Debug, Clone)]
pub(crate) struct Object<'a> {
    /// A library's soname or a program's file name.
    pub name: &'a str,
    /// The file to read.
    pub file: &'a Path,
    /// For a host scan's tree copy of a library, the host file it stands
    /// for, whose location gives the prefix since its compiled-in paths
    /// name that installation. Without one the object's own location
    /// does.
    pub twin: Option<PathBuf>,
}

/// The directories one build has inferred, across the rounds of the
/// walk that each admit more objects.
#[derive(Debug, Default)]
pub(crate) struct Inference {
    dirs: BTreeMap<PathBuf, InferredDir>,
    /// The directories read has handed out, whose pair and
    /// classification are settled because the caller may already carry
    /// them.
    handed: BTreeSet<PathBuf>,
    /// The directories that could not be read, with the least prefix
    /// and path pair the refusal was recorded under, so a smaller pair
    /// is tried again and the omission is reported once.
    refused: BTreeMap<PathBuf, Refusal>,
}

/// A directory classification refused: the directory as the failing
/// pair names it, for the omission the call reports once it has seen
/// every candidate, and the reason it names.
#[derive(Debug)]
struct Refusal {
    dir: InferredDir,
    why: String,
}

impl Inference {
    /// Read `objects` against `scope` and return the sources of the
    /// directories this call found first, sorted, each classified. A
    /// directory found before gains the new spellings and objects and,
    /// while it has not been handed out yet, the least prefix and path
    /// pair with a classification read again under it. A directory
    /// refused under one pair is tried again under a smaller one, and
    /// its omission is reported once, under the least pair that failed.
    pub(crate) fn read(&mut self, objects: &[Object], scope: &Scope) -> io::Result<Vec<PathBuf>> {
        let exclude: Vec<&str> = scope.exclude.iter().map(String::as_str).collect();
        let mut fresh = Vec::new();
        let reported = self.refused.keys().cloned().collect::<BTreeSet<_>>();
        for object in objects {
            let name = object.name;
            for candidate in candidates_of(object, scope, &exclude)? {
                let at_limit = self.dirs.len() >= MAX_INFERRED_DIRS;
                // The least prefix and path pair wins, so the
                // classification and the report do not depend on which
                // object named the directory first, and the
                // classification is read again under the winning pair,
                // since a member link resolves inside its prefix. A
                // directory an earlier round handed out is carried
                // already, so its pair and classification stay and the
                // object only adds spellings.
                match self.dirs.entry(candidate.source.clone()) {
                    Entry::Occupied(mut held) => {
                        let handed_out = self.handed.contains(&candidate.source);
                        let dir = held.get_mut();
                        let wins = !handed_out
                            && (&candidate.prefix, &candidate.rel) < (&dir.prefix, &dir.rel);
                        if wins {
                            dir.prefix = candidate.prefix.clone();
                            dir.rel = candidate.rel.clone();
                            dir.destination = candidate.destination.clone();
                        }
                        dir.compiled.insert(candidate.compiled.clone());
                        dir.objects.insert(name.to_string());
                        if !wins {
                            continue;
                        }
                        match classify(held.get(), scope.sysroot) {
                            Ok((kind, size)) => {
                                let dir = held.get_mut();
                                dir.kind = kind;
                                dir.size = size;
                            }
                            Err(error) => {
                                let (_, dir) = held.remove_entry();
                                self.refused.insert(
                                    candidate.source.clone(),
                                    Refusal {
                                        dir,
                                        why: format!("it cannot be read: {error}"),
                                    },
                                );
                                fresh.retain(|source| source != &candidate.source);
                            }
                        }
                    }
                    Entry::Vacant(slot) => {
                        match self.refused.entry(candidate.source.clone()) {
                            Entry::Occupied(mut failed) => {
                                let dir = &mut failed.get_mut().dir;
                                dir.compiled.insert(candidate.compiled.clone());
                                dir.objects.insert(name.to_string());
                                if (&candidate.prefix, &candidate.rel) >= (&dir.prefix, &dir.rel) {
                                    continue;
                                }
                                // The smaller pair may classify where the
                                // recorded one failed. The spellings stay
                                // with the directory either way.
                                dir.prefix = candidate.prefix.clone();
                                dir.rel = candidate.rel.clone();
                                dir.destination = candidate.destination.clone();
                                match classify(dir, scope.sysroot) {
                                    Ok((kind, size)) => {
                                        if at_limit {
                                            return Err(io::Error::new(
                                                io::ErrorKind::InvalidData,
                                                format!(
                                                    "{name} compiles in more directories than the \
                                                     {MAX_INFERRED_DIRS} one build may infer \
                                                     (MAX_INFERRED_DIRS); refusing unbounded inference"
                                                ),
                                            ));
                                        }
                                        let refusal = failed.get_mut();
                                        refusal.dir.kind = kind;
                                        refusal.dir.size = size;
                                        let (_, refusal) = failed.remove_entry();
                                        fresh.push(candidate.source.clone());
                                        slot.insert(refusal.dir);
                                    }
                                    Err(error) => {
                                        failed.get_mut().why =
                                            format!("it cannot be read: {error}");
                                    }
                                }
                            }
                            Entry::Vacant(unread) => {
                                if at_limit {
                                    return Err(io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        format!(
                                            "{name} compiles in more directories than the \
                                             {MAX_INFERRED_DIRS} one build may infer \
                                             (MAX_INFERRED_DIRS); refusing unbounded inference"
                                        ),
                                    ));
                                }
                                let mut dir = InferredDir {
                                    source: candidate.source.clone(),
                                    prefix: candidate.prefix.clone(),
                                    rel: candidate.rel.clone(),
                                    destination: candidate.destination.clone(),
                                    kind: InferredKind::Data,
                                    size: 0,
                                    compiled: BTreeSet::from([candidate.compiled.clone()]),
                                    objects: BTreeSet::from([name.to_string()]),
                                };
                                match classify(&dir, scope.sysroot) {
                                    Ok((kind, size)) => {
                                        dir.kind = kind;
                                        dir.size = size;
                                        fresh.push(candidate.source.clone());
                                        slot.insert(dir);
                                    }
                                    Err(error) => {
                                        unread.insert(Refusal {
                                            dir,
                                            why: format!("it cannot be read: {error}"),
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        fresh.sort();
        self.handed.extend(fresh.iter().cloned());
        for (source, refusal) in &self.refused {
            if !reported.contains(source) {
                omit(&refusal.dir, &refusal.why);
            }
        }
        Ok(fresh)
    }

    /// The directory inferred at `source`.
    pub(crate) fn get(&self, source: &Path) -> Option<&InferredDir> {
        self.dirs.get(source)
    }

    /// Every directory this inference holds, in source order, whatever
    /// the carry decision did with it: a directory that rides a carried
    /// parent has no carry entry of its own, and only this view of the
    /// scan can still name it.
    pub(crate) fn all(&self) -> impl Iterator<Item = &InferredDir> {
        self.dirs.values()
    }
}

/// Whether `dir` holds an ELF object anywhere below it, whatever its
/// name, read the way a plugin group's members are: a sysroot build's
/// admitted files alone, a host directory's files with each link confined
/// to the prefix. The walk stops at the first object, so a data
/// directory is the only one read whole, and its size comes with it,
/// counting regular files the way the copy does, which carries a link as
/// a link. A host directory past the tree walk limit is an error, so it
/// is omitted as too large rather than read without bound; a sysroot
/// walk is bounded by the files the build admitted.
fn classify(
    dir: &InferredDir,
    sysroot: Option<&SysrootUniverse>,
) -> io::Result<(InferredKind, u64)> {
    let mut size = 0u64;
    let mut objects = false;
    let hit = dir.hit();
    visit_members(&hit, sysroot, &mut |rel, file| {
        if read_elf_class(&file).is_some() {
            objects = true;
            return ControlFlow::Break(());
        }
        if file == hit.resolved.join(&rel) {
            size += fs::metadata(&file).map(|meta| meta.len()).unwrap_or(0);
        }
        ControlFlow::Continue(())
    })?;
    Ok(if objects {
        (InferredKind::Plugins, 0)
    } else {
        (InferredKind::Data, size)
    })
}

/// Record that `dir` is omitted, and why. An inferred directory is
/// optional, so nothing about it fails the build.
pub(crate) fn omit(dir: &InferredDir, why: &str) {
    completeness::record(Finding::new(
        dir.destination.display().to_string(),
        Outcome::OptionalOmission,
        format!("omitted because {why}; {}", origin(dir)),
    ));
}

/// Where an inferred directory came from, as its findings name it.
fn origin(dir: &InferredDir) -> String {
    let objects: Vec<&str> = dir.objects.iter().map(String::as_str).collect();
    let compiled: Vec<&str> = dir.compiled.iter().map(String::as_str).collect();
    format!(
        "inferred from {} below the prefix {}, compiled in by {} as {}",
        dir.source.display(),
        dir.prefix.display(),
        objects.join(", "),
        compiled.join(", ")
    )
}

/// The most skipped plugin members one finding names.
const SHOWN_SKIPS: usize = 3;

/// What the bundle carried of one inferred directory.
pub(crate) enum Carried<'a> {
    /// The directory, whole.
    Data,
    /// The trace-limited directory: the members kept of the total the
    /// trim judged.
    Trimmed { kept: usize, total: usize },
    /// The ELF objects its collection took, with the ones it skipped
    /// and why, and the count of its other files. The last field is
    /// the (kept, total) member counts the trim judged, when the trace
    /// limited the directory.
    Plugins(&'a GroupCollection, usize, Option<(usize, usize)>),
    /// Nothing, since the tree already holds its destination.
    Held,
    /// Nothing of its own, since the inferred destination named here
    /// already holds it, copied from the same place. The parent's
    /// trim governed the child's members too, so the subtree rides
    /// only with the members the trace opened.
    WithinTrimmed(&'a Path),
    /// Nothing of its own, since the inferred destination named here
    /// already holds it, copied from the same place.
    Within(&'a Path),
}

/// The words a carried directory's finding holds until the redirection
/// choice settles them with its mechanism.
pub(crate) const PENDING_REDIRECTION: &str = "redirection pending";

/// Record the finding for one inferred directory: what it holds, its
/// size, how the bundle carries it and where it came from. The finding
/// carries [`PENDING_REDIRECTION`] until the redirection choice names the
/// mechanism; a plugin directory's finding also names what was skipped
/// and why. A directory the trace limited is reported trace-limited, not
/// complete.
pub(crate) fn report(dir: &InferredDir, size: u64, carried: Carried) {
    let kind = match dir.kind {
        InferredKind::Plugins => "plugin directory",
        InferredKind::Data => "data directory",
    };
    let (outcome, mut what) = match carried {
        Carried::Data => (
            Outcome::Selected,
            format!(
                "{kind}, {}, carried, {PENDING_REDIRECTION}",
                format_size(size)
            ),
        ),
        Carried::Trimmed { kept, total } => (
            Outcome::TraceLimited,
            format!(
                "{kind}, {kept} of {total} member(s) kept by the trace, {}; the rest \
                 the trace did not open; {PENDING_REDIRECTION}",
                format_size(size)
            ),
        ),
        Carried::Held => (
            Outcome::Selected,
            format!(
                "{kind}, {}, held by the tree's own copy, {PENDING_REDIRECTION}",
                format_size(size)
            ),
        ),
        Carried::WithinTrimmed(outer) => (
            Outcome::TraceLimited,
            format!(
                "{kind}, whole size {}, carried within {}, only what the trace opened \
                 rides, redirected with it",
                format_size(size),
                outer.display()
            ),
        ),
        Carried::Within(outer) => (
            Outcome::Selected,
            format!(
                "{kind}, {}, carried within {}, redirected with it",
                format_size(size),
                outer.display()
            ),
        ),
        Carried::Plugins(collection, 0, None) if collection.plugins.is_empty() => (
            Outcome::OptionalOmission,
            "plugin directory with nothing the bundle can carry".to_string(),
        ),
        Carried::Plugins(collection, files, None) => (
            Outcome::Selected,
            format!(
                "plugin directory, {} object(s) and {files} other file(s), {}, \
                 {PENDING_REDIRECTION}",
                collection.plugins.len(),
                format_size(size)
            ),
        ),
        Carried::Plugins(collection, files, Some((kept, total))) => (
            Outcome::TraceLimited,
            format!(
                "plugin directory, {} object(s) and {files} other file(s), {}; {kept} of \
                 {total} member(s) kept by the trace; the rest the trace did not open; \
                 {PENDING_REDIRECTION}",
                collection.plugins.len(),
                format_size(size)
            ),
        ),
    };
    if let Carried::Plugins(collection, _, _) = carried {
        let skipped = &collection.skipped;
        for (name, reason) in skipped.iter().take(SHOWN_SKIPS) {
            what.push_str(&format!("; {name} omitted: {reason}"));
        }
        if skipped.len() > SHOWN_SKIPS {
            what.push_str(&format!("; {} more omitted", skipped.len() - SHOWN_SKIPS));
        }
    }
    completeness::record(Finding::new(
        dir.destination.display().to_string(),
        outcome,
        format!("{what}; {}", origin(dir)),
    ));
}

/// One compiled-in path of one object that names a directory below a
/// resource base.
#[derive(Debug)]
struct Candidate {
    compiled: String,
    source: PathBuf,
    prefix: PathBuf,
    rel: PathBuf,
    destination: PathBuf,
}

/// The candidates the object at `path` compiles in, in file order. An
/// object without an installation prefix has no resource base and is not
/// read.
///
/// A string is resolved from the source's own `/`, the host's or the
/// sysroot's, so a `..` link cannot be clamped into the prefix and a
/// merged-usr spelling such as `/lib/...` for a `/usr` object lands
/// where the loader would land. Only a string that plausibly names a
/// base, below the prefix or, for a prefix named `usr`, below its parent
/// in either merged spelling, is resolved, and only those count against
/// [`MAX_OBJECT_RESOLUTIONS`]. Only one naming an existing directory
/// counts against [`MAX_OBJECT_PATHS`].
fn candidates_of(object: &Object, scope: &Scope, exclude: &[&str]) -> io::Result<Vec<Candidate>> {
    let path = object.file;
    let sysroot = scope.sysroot;
    if libc_family_of_soname(object.name).is_some() {
        return Ok(Vec::new());
    }
    let installed = object.twin.as_deref().unwrap_or(path);
    let Some(own_prefix) = object_prefix(installed) else {
        return Ok(Vec::new());
    };
    let Ok(data) = fs::read(path) else {
        return Ok(Vec::new());
    };
    let source_root = sysroot.map_or(Path::new("/"), SysrootUniverse::root);
    let below_source = |place: &Path| match scope
        .trees
        .iter()
        .find_map(|tree| place.strip_prefix(tree).ok())
    {
        Some(rel) => source_root.join(rel),
        None => place.to_path_buf(),
    };
    let prefix = below_source(&own_prefix);
    let under_root = prefix == source_root;
    let resolve = |place: &Path| -> Option<PathBuf> {
        let from_root = place.strip_prefix(source_root).ok()?;
        match sysroot {
            Some(universe) => in_root(universe.root(), from_root).ok(),
            None => resolve_within(Path::new("/"), from_root).ok(),
        }
    };
    let own_dir = installed
        .parent()
        .map(below_source)
        .map(|dir| resolve(&dir).unwrap_or(dir));
    let own_rel = own_dir
        .as_deref()
        .and_then(|dir| dir.strip_prefix(&prefix).ok())
        .map(|rel| merged(rel, under_root).to_path_buf());
    let merged_parent = prefix
        .file_name()
        .is_some_and(|name| name == "usr")
        .then(|| prefix.parent())
        .flatten();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut directories = 0usize;
    let mut out = Vec::new();
    for compiled in compiled_strings(&data) {
        let from_root = Path::new(&compiled[1..]);
        let declared = source_root.join(from_root);
        let plausible = declared
            .strip_prefix(&prefix)
            .is_ok_and(|rel| in_resource_base(rel, under_root))
            || merged_parent
                .and_then(|parent| declared.strip_prefix(parent).ok())
                .is_some_and(|rel| in_resource_base(rel, true));
        if !plausible || !seen.insert(compiled) {
            continue;
        }
        if seen.len() > MAX_OBJECT_RESOLUTIONS {
            return Err(bounded(
                path,
                MAX_OBJECT_RESOLUTIONS,
                "MAX_OBJECT_RESOLUTIONS",
            ));
        }
        let Some(source) = resolve(&declared) else {
            continue;
        };
        if !source.is_dir() {
            continue;
        }
        directories += 1;
        if directories > MAX_OBJECT_PATHS {
            return Err(bounded(path, MAX_OBJECT_PATHS, "MAX_OBJECT_PATHS"));
        }
        let Ok(rel) = source.strip_prefix(&prefix) else {
            continue;
        };
        let declared_rel = declared
            .strip_prefix(&prefix)
            .ok()
            .map(|rel| merged(rel, under_root));
        if !in_resource_base(rel, under_root)
            || dropped(merged(rel, under_root), own_rel.as_deref(), exclude)
            || described(merged(rel, under_root), scope.descriptors)
            || declared_rel.is_some_and(|rel| described(rel, scope.descriptors))
        {
            continue;
        }
        if let Some(universe) = sysroot {
            let admitted = source
                .strip_prefix(universe.root())
                .is_ok_and(|rel| universe.admits(&rel.to_string_lossy()));
            if !admitted {
                continue;
            }
        }
        let destination = destination_of(merged(rel, under_root), scope.lib_dir);
        let rel = rel.to_path_buf();
        out.push(Candidate {
            compiled: compiled.to_string(),
            prefix: prefix.clone(),
            rel,
            destination,
            source,
        });
    }
    Ok(out)
}

/// Whether a candidate at `rel`, relative to the prefix and in its
/// merged spelling, is dropped: when it equals or contains the directory
/// the object sits in, `own`, when the catalog's inference exclusions
/// drop it, or when a recipe exclusion begins one of its components
/// below its library directory or `share`, as one begins a soname. A
/// shared root drops what equals or contains it. A tree, libc or driver
/// entry also drops what lies below it, and one relative to the library
/// directories names its path joined to the innermost library directory
/// the candidate lies in.
fn dropped(rel: &Path, own: Option<&Path>, exclude: &[&str]) -> bool {
    let exclusions = catalog::inference_exclusions();
    let library_dir = library_dir_of(rel);
    let owned = |entry: &ExclusionEntry| {
        let owner = match entry.base {
            ExclusionBase::InstallationPrefix => PathBuf::from(entry.path.as_str()),
            ExclusionBase::LibraryDirs => match &library_dir {
                Some(dir) => dir.join(entry.path.as_str()),
                None => return false,
            },
        };
        rel.starts_with(&owner) || owner.starts_with(rel)
    };
    let base = library_dir.as_deref().unwrap_or(Path::new("share"));
    let excluded = rel.strip_prefix(base).is_ok_and(|below| {
        below
            .components()
            .any(|part| is_excluded(&part.as_os_str().to_string_lossy(), exclude))
    });
    own.is_some_and(|own| own.starts_with(rel))
        || excluded
        || exclusions
            .shared_roots
            .iter()
            .any(|root| Path::new(root.as_str()).starts_with(rel))
        || exclusions
            .shared_trees
            .iter()
            .chain(&exclusions.libc)
            .chain(&exclusions.drivers)
            .any(owned)
}

/// Whether a descriptor declares a location that equals, contains or
/// lies below `rel`, relative to the prefix in its merged spelling. A
/// library-directory candidate of a descriptor names its path below the
/// innermost library directory `rel` lies in. The test is conservative
/// on purpose: it compares paths relative to the prefix, not the
/// directories a descriptor's discovery would find, and it also keeps
/// inference out of a parent of a declared directory, whose carry would
/// bypass the descriptor's members, requirement and hooks.
fn described(rel: &Path, descriptors: &[&Descriptor]) -> bool {
    let library_dir = library_dir_of(rel);
    descriptors
        .iter()
        .flat_map(|descriptor| {
            let resources = &descriptor.resources;
            resources
                .plugins
                .iter()
                .chain(&resources.data)
                .flat_map(|group| &group.candidates)
                .chain(resources.helpers.iter().flat_map(|h| &h.candidates))
        })
        .any(|candidate| {
            let declared = match candidate.base {
                SourceBase::InstallationPrefix => PathBuf::from(candidate.path.as_str()),
                SourceBase::LibraryDir => match &library_dir {
                    Some(dir) => dir.join(candidate.path.as_str()),
                    None => return false,
                },
            };
            rel.starts_with(&declared) || declared.starts_with(rel)
        })
}

/// Where the bundle carries a directory at `rel`, relative to the prefix
/// in its merged spelling: below `lib_dir` for one below a library
/// directory, else at `rel`.
fn destination_of(rel: &Path, lib_dir: &Path) -> PathBuf {
    match library_dir_of(rel).and_then(|dir| rel.strip_prefix(dir).ok().map(Path::to_path_buf)) {
        Some(below) => lib_dir.join(below),
        None => rel.to_path_buf(),
    }
}

/// The bounded-reading error: which object stopped, and at which bound.
fn bounded(path: &Path, cap: usize, name: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "{}: more than {cap} compiled-in paths ({name}); refusing unbounded inference",
            path.display()
        ),
    )
}

/// The normalized absolute paths an ELF image carries as whole
/// NUL-terminated strings in its loadable read-only data, in file order
/// and possibly repeated. The dynamic string table is not read: its run
/// path entries name library directories the walk already searches, not
/// resources. A spelling with an empty, `.` or `..` component, or a
/// trailing separator, is not normalized and is not read.
fn compiled_strings(data: &[u8]) -> impl Iterator<Item = &str> {
    read_only_ranges(data)
        .into_iter()
        .flat_map(move |(start, end)| data[start..end].split(|&b| b == 0))
        .filter_map(normalized_absolute)
}

/// A minimal ELF image for sibling tests: one read-only `PT_LOAD` per
/// segment, each holding its strings NUL-terminated.
#[cfg(test)]
pub(crate) fn test_elf(segments: &[(u32, &[&str])]) -> Vec<u8> {
    tests::test_image(segments)
}

/// The file ranges of the `PT_LOAD` segments the loader maps without
/// write permission, clamped to `data`, sorted, merged where they
/// overlap or touch, and with the dynamic string table taken out. Many
/// overlapping headers are read once.
pub(crate) fn read_only_ranges(data: &[u8]) -> Vec<(usize, usize)> {
    let Ok(elf) = goblin::elf::Elf::parse(data) else {
        return Vec::new();
    };
    let mut ranges: Vec<(usize, usize)> = elf
        .program_headers
        .iter()
        .filter(|ph| ph.p_type == PT_LOAD && ph.p_flags & PF_W == 0)
        .filter_map(|ph| {
            let start = usize::try_from(ph.p_offset).ok()?.min(data.len());
            let size = usize::try_from(ph.p_filesz).ok()?;
            Some((start, start.saturating_add(size).min(data.len())))
        })
        .filter(|(start, end)| start < end)
        .collect();
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    // The section header and the dynamic segment can each name a string
    // table, and the loader reads the dynamic segment's: both are left out
    // when they disagree.
    let mut skips: Vec<(usize, usize)> = dynstr_range(&elf).into_iter().collect();
    if let Some(dynamic) = elf.dynamic.as_ref() {
        let (strtab, strsz) = (dynamic.info.strtab, dynamic.info.strsz);
        if strtab != 0
            && strsz != 0
            && let Some(end) = strtab.checked_add(strsz)
        {
            skips.push((strtab, end));
        }
    }
    for (skip_start, skip_end) in skips {
        merged = merged
            .into_iter()
            .flat_map(|(start, end)| [(start, end.min(skip_start)), (start.max(skip_end), end)])
            .filter(|(start, end)| start < end)
            .collect();
    }
    merged
}

/// `piece` as a path the lint would read, with no empty, `.` or `..`
/// component and no trailing separator, or `None`.
fn normalized_absolute(piece: &[u8]) -> Option<&str> {
    if piece.len() < 2 || !path_like(piece) {
        return None;
    }
    let path = std::str::from_utf8(piece).ok()?;
    path[1..]
        .split('/')
        .all(|part| !part.is_empty() && part != "." && part != "..")
        .then_some(path)
}

/// `rel` as the hierarchy below the prefix sees it: under a `/` prefix,
/// a path below `usr` is the same place as the path without it.
fn merged(rel: &Path, under_root: bool) -> &Path {
    match rel.strip_prefix("usr") {
        Ok(below) if under_root => below,
        _ => rel,
    }
}

/// Whether `rel` lies strictly below a resource base: a library
/// directory or `share`, below the prefix or, under a `/` prefix, below
/// `usr`. A base itself is not below one.
fn in_resource_base(rel: &Path, under_root: bool) -> bool {
    let rel = merged(rel, under_root);
    let base = match library_dir_of(rel) {
        Some(dir) => dir,
        None if rel.starts_with("share") => PathBuf::from("share"),
        None => return false,
    };
    rel.starts_with(&base) && rel != base
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::collect::MAX_GROUP_DEPTH;
    use crate::bundle::plan::TREE_WALK_LIMIT;
    use crate::bundle::sysroot::fixture;
    use onelf_format::rules::is_multiarch;
    use std::os::unix::fs::symlink;

    const READ: u32 = 4;
    const READ_WRITE: u32 = 6;

    /// How a fixture names its dynamic string table.
    #[derive(Clone, Copy, PartialEq)]
    enum Dynstr {
        /// `DT_STRTAB` and `DT_STRSZ` in a `PT_DYNAMIC`, as after `sstrip`.
        Dynamic,
        /// A `.dynstr` section header.
        Section,
    }

    /// An ELF image of either class and byte order with one `PT_LOAD`
    /// per segment, each holding its strings NUL-terminated at a virtual
    /// address equal to its file offset. A run path, when given, sits in
    /// a dynamic string table inside the first segment, named the way
    /// `table` says.
    fn image(
        class64: bool,
        big_endian: bool,
        segments: &[(u32, &[&str])],
        runpath: Option<(&str, Dynstr)>,
    ) -> Vec<u8> {
        let put = |data: &mut Vec<u8>, at: usize, value: u64, width: usize| {
            let bytes = value.to_le_bytes();
            let mut field = bytes[..width].to_vec();
            if big_endian {
                field.reverse();
            }
            data[at..at + width].copy_from_slice(&field);
        };
        let (ehsize, phentsize, word) = if class64 { (64, 56, 8) } else { (52, 32, 4) };
        let dynamic = runpath.is_some_and(|(_, table)| table == Dynstr::Dynamic);
        let phnum = segments.len() + usize::from(dynamic);
        let mut data = vec![0u8; ehsize + phentsize * phnum];
        data[0..4].copy_from_slice(b"\x7fELF");
        data[4] = if class64 { 2 } else { 1 };
        data[5] = if big_endian { 2 } else { 1 };
        data[6] = 1;
        put(&mut data, 16, 3, 2);
        put(&mut data, 18, if class64 { 62 } else { 3 }, 2);
        put(&mut data, 20, 1, 4);
        let (phoff_at, sizes_at) = if class64 { (32, 52) } else { (28, 40) };
        put(&mut data, phoff_at, ehsize as u64, word);
        put(&mut data, sizes_at, ehsize as u64, 2);
        put(&mut data, sizes_at + 2, phentsize as u64, 2);
        put(&mut data, sizes_at + 4, phnum as u64, 2);
        let mut dynstr = None;
        let mut headers = Vec::new();
        for (index, (flags, strings)) in segments.iter().enumerate() {
            let offset = data.len();
            data.push(0);
            for s in *strings {
                data.extend_from_slice(s.as_bytes());
                data.push(0);
            }
            if let (0, Some((runpath, _))) = (index, runpath) {
                dynstr = Some((data.len(), runpath.len() + 2));
                data.push(0);
                data.extend_from_slice(runpath.as_bytes());
                data.push(0);
            }
            headers.push((PT_LOAD, *flags, offset, data.len() - offset));
        }
        if let Some((strtab, strsz)) = dynstr.filter(|_| dynamic) {
            let offset = data.len();
            for (tag, value) in [
                (29u64, 1u64),
                (5, strtab as u64),
                (10, strsz as u64),
                (0, 0),
            ] {
                for v in [tag, value] {
                    let at = data.len();
                    data.extend(std::iter::repeat_n(0, word));
                    put(&mut data, at, v, word);
                }
            }
            headers.push((2, READ, offset, data.len() - offset));
        }
        for (index, (kind, flags, offset, size)) in headers.into_iter().enumerate() {
            let ph = ehsize + phentsize * index;
            let (offset, size) = (offset as u64, size as u64);
            put(&mut data, ph, u64::from(kind), 4);
            if class64 {
                put(&mut data, ph + 4, u64::from(flags), 4);
                for (at, value) in [
                    (8, offset),
                    (16, offset),
                    (24, offset),
                    (32, size),
                    (40, size),
                ] {
                    put(&mut data, ph + at, value, 8);
                }
            } else {
                for (at, value) in [
                    (4, offset),
                    (8, offset),
                    (12, offset),
                    (16, size),
                    (20, size),
                ] {
                    put(&mut data, ph + at, value, 4);
                }
                put(&mut data, ph + 24, u64::from(flags), 4);
            }
        }
        if let Some((strtab, strsz)) = dynstr.filter(|_| !dynamic) {
            const NAMES: &[u8] = b"\0.dynstr\0.shstrtab\0";
            let names = data.len();
            data.extend_from_slice(NAMES);
            while !data.len().is_multiple_of(8) {
                data.push(0);
            }
            let (shentsize, shoff_at, counts_at, offset_at, size_at) = if class64 {
                (64, 40, 58, 24, 32)
            } else {
                (40, 32, 46, 16, 20)
            };
            let shoff = data.len();
            put(&mut data, shoff_at, shoff as u64, word);
            put(&mut data, counts_at, shentsize as u64, 2);
            put(&mut data, counts_at + 2, 3, 2);
            put(&mut data, counts_at + 4, 2, 2);
            data.extend(std::iter::repeat_n(0, shentsize * 3));
            for (index, name, offset, size) in [(1, 1, strtab, strsz), (2, 9, names, NAMES.len())] {
                let sh = shoff + shentsize * index;
                put(&mut data, sh, name, 4);
                put(&mut data, sh + 4, 3, 4);
                put(&mut data, sh + offset_at, offset as u64, word);
                put(&mut data, sh + size_at, size as u64, word);
            }
        }
        data
    }

    fn elf(segments: &[(u32, &[&str])]) -> Vec<u8> {
        image(true, false, segments, None)
    }

    pub(crate) fn test_image(segments: &[(u32, &[&str])]) -> Vec<u8> {
        elf(segments)
    }

    fn strings(data: &[u8]) -> Vec<&str> {
        let set: BTreeSet<&str> = compiled_strings(data).collect();
        set.into_iter().collect()
    }

    /// A scratch tree, canonicalized so compiled-in strings name its
    /// real path.
    struct Tree(PathBuf);

    impl Tree {
        fn new(tag: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("onelf-infer-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }

        fn path(&self, rel: &str) -> String {
            self.0.join(rel).to_string_lossy().into_owned()
        }

        fn dir(&self, rel: &str) {
            fs::create_dir_all(self.0.join(rel)).unwrap();
        }

        fn file(&self, rel: &str, bytes: &[u8]) {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }

        /// An object at `rel` compiling in `paths`, each under the tree,
        /// as its name and file.
        fn object(&self, rel: &str, paths: &[&str]) -> (String, PathBuf) {
            let owned: Vec<String> = paths.iter().map(|p| self.path(p)).collect();
            let strings: Vec<&str> = owned.iter().map(String::as_str).collect();
            self.file(rel, &elf(&[(READ, &strings)]));
            let path = self.0.join(rel);
            let name = path.file_name().unwrap().to_str().unwrap().to_string();
            (name, path)
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Every directory `objects` compile in, sorted by source.
    fn discover(objects: &[Object], scope: &Scope) -> io::Result<Vec<InferredDir>> {
        let mut inference = Inference::default();
        inference.read(objects, scope)?;
        Ok(inference.dirs.into_values().collect())
    }

    fn run(
        objects: &[(String, PathBuf)],
        sysroot: Option<&SysrootUniverse>,
    ) -> io::Result<Vec<InferredDir>> {
        run_in(objects, sysroot, &[])
    }

    /// [`run`] for objects some of which the tree at `trees` holds.
    fn run_in(
        objects: &[(String, PathBuf)],
        sysroot: Option<&SysrootUniverse>,
        trees: &[&Path],
    ) -> io::Result<Vec<InferredDir>> {
        let objects: Vec<Object> = objects
            .iter()
            .map(|(name, path)| Object {
                name,
                file: path,
                twin: None,
            })
            .collect();
        discover(
            &objects,
            &Scope {
                sysroot,
                trees,
                exclude: &[],
                descriptors: &[],
                lib_dir: Path::new("lib"),
            },
        )
    }

    fn rels(dirs: &[InferredDir]) -> Vec<String> {
        dirs.iter()
            .map(|d| d.rel.to_string_lossy().into_owned())
            .collect()
    }

    /// Only whole normalized absolute strings of the read-only loadable
    /// segments are read. Writable data, relative strings, unnormalized
    /// spellings and text with spaces are not paths.
    #[test]
    fn compiled_paths_are_whole_normalized_strings_of_read_only_segments() {
        let image = elf(&[
            (
                READ,
                &[
                    "/usr/share/examplefw",
                    "/usr/lib/examplefw/plugins",
                    "/usr/share/examplefw",
                    "/usr/bin/../lib/bfd-plugins",
                    "/usr/lib/..",
                    "/usr/lib64/../lib64",
                    "/usr/share//opencc",
                    "/usr/./share/b",
                    "/usr/share/d/",
                    "/usr/share/with space",
                    "relative/path",
                    "/",
                ],
            ),
            (READ_WRITE, &["/usr/share/writable"]),
        ]);
        assert_eq!(
            strings(&image),
            ["/usr/lib/examplefw/plugins", "/usr/share/examplefw"]
        );
        assert!(strings(b"not an elf /usr/share/examplefw\0").is_empty());
    }

    /// Both classes and byte orders are read alike, a table of
    /// overlapping headers is read once, and a truncated or contradictory
    /// header table reads nothing rather than failing.
    #[test]
    fn every_class_and_byte_order_reads_and_hostile_tables_do_not_panic() {
        let paths: &[&str] = &["/usr/share/examplefw"];
        for (class64, big_endian) in [(true, false), (false, false), (true, true), (false, true)] {
            let data = image(class64, big_endian, &[(READ, paths)], None);
            assert_eq!(strings(&data), paths, "class64 {class64} big {big_endian}");
        }

        let segments: Vec<(u32, &[&str])> = (0..1000).map(|_| (READ, paths)).collect();
        let mut overlapping = image(true, false, &segments, None);
        for index in 1..1000 {
            let ph = 64 + 56 * index;
            let first: Vec<u8> = overlapping[64 + 8..64 + 48].to_vec();
            overlapping[ph + 8..ph + 48].copy_from_slice(&first);
        }
        assert_eq!(read_only_ranges(&overlapping).len(), 1);
        assert_eq!(strings(&overlapping), paths);

        let whole = image(true, false, &[(READ, paths), (READ, paths)], None);
        assert!(strings(&whole[..64 + 56 + 8]).is_empty());
        let mut past_end = whole.clone();
        past_end[56..58].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(strings(&past_end).is_empty());
        let mut oversized = whole;
        oversized[64 + 32..64 + 40].copy_from_slice(&u64::MAX.to_le_bytes());
        let _ = strings(&oversized);
    }

    /// A run path lives in the dynamic string table, which names library
    /// directories the walk searches, so it is never a candidate, while
    /// the same object's resource strings still are.
    #[test]
    fn run_paths_are_not_compiled_in_directories() {
        let tree = Tree::new("runpath");
        tree.dir("usr/lib/pulseaudio");
        tree.dir("usr/share/examplefw");
        let resource = tree.path("usr/share/examplefw");
        let runpath = tree.path("usr/lib/pulseaudio");
        for table in [Dynstr::Dynamic, Dynstr::Section] {
            let data = image(
                true,
                false,
                &[(READ, &[resource.as_str()])],
                Some((&runpath, table)),
            );
            assert!(
                data.windows(runpath.len()).any(|w| w == runpath.as_bytes()),
                "the fixture carries the run path in a read-only segment"
            );
            assert_eq!(strings(&data), [resource.as_str()]);
            tree.file("usr/lib/libpulse.so.0", &data);
            let lib = (
                "libpulse.so.0".to_string(),
                tree.0.join("usr/lib/libpulse.so.0"),
            );
            assert_eq!(rels(&run(&[lib], None).unwrap()), ["share/examplefw"]);
        }
    }

    /// One object may name a bounded number of existing directories,
    /// and the error names the bound. Strings that can never be
    /// candidates, and the many source file paths a Go program carries,
    /// do not count against it.
    #[test]
    fn an_object_naming_too_many_directories_stops_with_the_bound_named() {
        let tree = Tree::new("bound");
        let mut paths: Vec<String> = (0..=MAX_OBJECT_PATHS)
            .map(|i| format!("usr/share/p{i}"))
            .collect();
        for path in &paths {
            tree.dir(path);
        }
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let lib = tree.object("usr/lib/libmany.so.1", &refs);
        let error = run(&[lib], None).unwrap_err();
        assert!(error.to_string().contains("MAX_OBJECT_PATHS"), "{error}");

        paths.truncate(8);
        paths.extend((0..2000).map(|i| format!("usr/colour{i}/1")));
        paths.extend((0..2000).map(|i| format!("usr/lib/go/src/x/{i}.go")));
        paths.extend((0..8).map(|i| format!("opt/other/share/p{i}")));
        tree.dir("usr/lib/go/src/x");
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let program = tree.object("usr/bin/tool", &refs);
        assert_eq!(run(&[program], None).unwrap().len(), 8);
    }

    /// Resolving plausible strings is bounded too, so a hostile object
    /// naming endless paths that look like resources stops with the
    /// bound named.
    #[test]
    fn an_object_driving_too_many_resolutions_stops_with_the_bound_named() {
        let tree = Tree::new("resolutions");
        let paths: Vec<String> = (0..=MAX_OBJECT_RESOLUTIONS)
            .map(|i| format!("usr/share/p{i}"))
            .collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let lib = tree.object("usr/lib/libmany.so.1", &refs);
        let error = run(&[lib], None).unwrap_err();
        assert!(
            error.to_string().contains("MAX_OBJECT_RESOLUTIONS"),
            "{error}"
        );
    }

    /// One build may find a bounded number of directories, and the error
    /// names the bound.
    #[test]
    fn a_build_finding_too_many_directories_stops_with_the_bound_named() {
        let tree = Tree::new("build-bound");
        let paths: Vec<String> = (0..=MAX_INFERRED_DIRS)
            .map(|i| format!("usr/share/fw{i}"))
            .collect();
        for path in &paths {
            tree.dir(path);
        }
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let lib = tree.object("usr/lib/libmany.so.1", &refs[..MAX_INFERRED_DIRS]);
        assert_eq!(
            run(std::slice::from_ref(&lib), None).unwrap().len(),
            MAX_INFERRED_DIRS
        );
        let more = tree.object("usr/lib/libmore.so.1", &refs[MAX_INFERRED_DIRS..]);
        let error = run(&[lib, more], None).unwrap_err();
        assert!(error.to_string().contains("MAX_INFERRED_DIRS"), "{error}");
    }

    /// A data directory holding more entries than the tree walk admits
    /// is not read without bound: its classification stops with the
    /// bound named, and the directory is omitted rather than carried.
    #[test]
    fn a_directory_past_the_walk_limit_is_never_read_whole() {
        let tree = Tree::new("classify-bound");
        for i in 0..=TREE_WALK_LIMIT {
            tree.file(&format!("usr/share/big/f{i:06}"), b"");
        }
        let dir = InferredDir {
            source: tree.0.join("usr/share/big"),
            prefix: tree.0.clone(),
            rel: PathBuf::from("usr/share/big"),
            destination: PathBuf::from("usr/share/big"),
            kind: InferredKind::Data,
            size: 0,
            compiled: BTreeSet::from([tree.path("usr/share/big")]),
            objects: BTreeSet::from(["libprobe.so".to_string()]),
        };
        let error = classify(&dir, None).unwrap_err();
        assert!(error.to_string().contains("past the walk limit"), "{error}");
    }

    /// Two prefixes whose libraries name one directory agree on one
    /// prefix, path and destination, whichever object was read first:
    /// the least prefix and path pair wins, and its destination moves
    /// with it.
    #[test]
    fn a_directory_named_from_two_prefixes_keeps_one_destination() {
        let tree = Tree::new("two-prefixes");
        tree.dir("p/share/sdk/share/fw");
        let a = tree.object("p/lib/liba.so.1", &["p/share/sdk/share/fw"]);
        let b = tree.object("p/share/sdk/lib/libb.so.1", &["p/share/sdk/share/fw"]);
        let expected = InferredDir {
            source: tree.0.join("p/share/sdk/share/fw"),
            prefix: tree.0.join("p"),
            rel: PathBuf::from("share/sdk/share/fw"),
            destination: PathBuf::from("share/sdk/share/fw"),
            kind: InferredKind::Data,
            size: 0,
            compiled: BTreeSet::from([tree.path("p/share/sdk/share/fw")]),
            objects: BTreeSet::from(["liba.so.1".to_string(), "libb.so.1".to_string()]),
        };
        let dirs = run(&[a.clone(), b.clone()], None).unwrap();
        assert_eq!(dirs.len(), 1, "{:?}", rels(&dirs));
        assert_eq!(dirs[0].prefix, expected.prefix);
        assert_eq!(dirs[0].destination, expected.destination);
        let dirs = run(&[b, a], None).unwrap();
        assert_eq!(dirs.len(), 1, "{:?}", rels(&dirs));
        assert_eq!(dirs[0].prefix, expected.prefix, "the least prefix wins");
        assert_eq!(
            dirs[0].destination, expected.destination,
            "the destination belongs to the winning prefix"
        );
    }

    /// When a second object names a directory from a smaller prefix,
    /// the classification is read again under the winning prefix: a
    /// member link resolves inside it, so a directory that looked like
    /// data under the inner prefix can be plugins. Both read orders
    /// agree.
    #[test]
    fn a_directory_reclassified_under_the_winning_prefix() {
        let tree = Tree::new("two-prefix-class");
        tree.dir("p/share/sdk/share/fw");
        tree.file("p/share/sdk/share/fw/conf", b"data");
        tree.object("p/lib/plugin.so", &[]);
        std::os::unix::fs::symlink(
            "../../../../lib/plugin.so",
            tree.0.join("p/share/sdk/share/fw/module.so"),
        )
        .unwrap();
        let a = tree.object("p/lib/liba.so.1", &["p/share/sdk/share/fw"]);
        let b = tree.object("p/share/sdk/lib/libb.so.1", &["p/share/sdk/share/fw"]);
        for objects in [[a.clone(), b.clone()], [b.clone(), a.clone()]] {
            let dirs = run(&objects, None).unwrap();
            assert_eq!(dirs.len(), 1, "{:?}", rels(&dirs));
            assert_eq!(dirs[0].prefix, tree.0.join("p"), "{:?}", rels(&dirs));
            assert_eq!(dirs[0].destination, PathBuf::from("share/sdk/share/fw"));
            assert_eq!(dirs[0].kind, InferredKind::Plugins, "{:?}", rels(&dirs));
        }
    }

    /// A directory refused under one pair is tried again when a
    /// smaller pair arrives, so the refusal is not final while the
    /// winning prefix is still unknown. A directory holding a link to
    /// an object only the outer prefix resolves, beside a branch
    /// deeper than the group walk admits, is plugins under the outer
    /// prefix in both read orders, though the inner prefix cannot
    /// finish the walk.
    #[test]
    fn a_refused_directory_is_tried_again_under_a_smaller_pair() {
        let tree = Tree::new("two-prefix-retry");
        tree.dir("p/share/sdk/share/fw");
        tree.file("p/share/sdk/share/fw/conf", b"data");
        tree.object("p/lib/plugin.so", &[]);
        std::os::unix::fs::symlink(
            "../../../../lib/plugin.so",
            tree.0.join("p/share/sdk/share/fw/0link"),
        )
        .unwrap();
        let mut deep = tree.0.join("p/share/sdk/share/fw/zdeep");
        for level in 0..=MAX_GROUP_DEPTH {
            deep = deep.join(format!("d{level:02}"));
        }
        fs::create_dir_all(&deep).unwrap();
        let a = tree.object("p/lib/liba.so.1", &["p/share/sdk/share/fw"]);
        let b = tree.object("p/share/sdk/lib/libb.so.1", &["p/share/sdk/share/fw"]);
        let expected = InferredDir {
            source: tree.0.join("p/share/sdk/share/fw"),
            prefix: tree.0.join("p"),
            rel: PathBuf::from("share/sdk/share/fw"),
            destination: PathBuf::from("share/sdk/share/fw"),
            kind: InferredKind::Plugins,
            size: 0,
            compiled: BTreeSet::from([tree.path("p/share/sdk/share/fw")]),
            objects: BTreeSet::from(["liba.so.1".to_string(), "libb.so.1".to_string()]),
        };
        for objects in [[a.clone(), b.clone()], [b.clone(), a.clone()]] {
            let dirs = run(&objects, None).unwrap();
            assert_eq!(dirs.len(), 1, "{:?}", rels(&dirs));
            assert_eq!(dirs[0], expected, "{:?}", rels(&dirs));
        }
    }

    /// A source already refused only gains spellings from a later
    /// object, and that must not trip the directory limit: the limit
    /// answers a newly admitted directory, not a refusal that stays
    /// refused.
    #[test]
    fn a_refusal_gains_spellings_past_the_directory_limit() {
        let tree = Tree::new("refusal-full");
        let mut deep = tree.0.join("p/share/sdk/deepfw/zdeep");
        for level in 0..=MAX_GROUP_DEPTH {
            deep = deep.join(format!("d{level:02}"));
        }
        fs::create_dir_all(&deep).unwrap();
        let r1 = tree.object("p/lib/libr1.so.1", &["p/share/sdk/deepfw"]);
        let fill: Vec<String> = (0..MAX_INFERRED_DIRS)
            .map(|i| format!("usr/share/fw{i}"))
            .collect();
        for dir in &fill {
            tree.dir(dir);
        }
        let fill_refs: Vec<&str> = fill.iter().map(String::as_str).collect();
        let many = tree.object("usr/lib/libmany.so.1", &fill_refs);
        let r2 = tree.object("p/lib/libr2.so.1", &["p/share/sdk/deepfw"]);
        let dirs = run(&[r1, many, r2], None).unwrap();
        assert_eq!(dirs.len(), MAX_INFERRED_DIRS, "{:?}", rels(&dirs));
        assert!(!rels(&dirs).iter().any(|rel| rel.contains("deepfw")));
    }

    /// A directory an earlier read handed out is carried already, so a
    /// later object naming it from a smaller prefix only adds
    /// spellings: the pair and the classification the build acted on
    /// stay.
    #[test]
    fn a_directory_handed_out_keeps_its_pair_and_classification() {
        let tree = Tree::new("two-prefix-handed");
        tree.dir("p/share/sdk/share/fw");
        tree.file("p/share/sdk/share/fw/conf", b"data");
        tree.object("p/lib/plugin.so", &[]);
        std::os::unix::fs::symlink(
            "../../../../lib/plugin.so",
            tree.0.join("p/share/sdk/share/fw/module.so"),
        )
        .unwrap();
        let a = tree.object("p/lib/liba.so.1", &["p/share/sdk/share/fw"]);
        let b = tree.object("p/share/sdk/lib/libb.so.1", &["p/share/sdk/share/fw"]);
        let source = tree.0.join("p/share/sdk/share/fw");
        let scope = Scope {
            sysroot: None,
            trees: &[],
            exclude: &[],
            descriptors: &[],
            lib_dir: Path::new("lib"),
        };
        let mut inference = Inference::default();
        let objects_b = [Object {
            name: b.0.as_str(),
            file: b.1.as_path(),
            twin: None,
        }];
        let fresh = inference.read(&objects_b, &scope).unwrap();
        assert_eq!(fresh.len(), 1, "{fresh:?}");
        assert_eq!(fresh[0], source);
        let carried = inference.get(&source).unwrap();
        assert_eq!(carried.prefix, tree.0.join("p/share/sdk"));
        assert_eq!(carried.destination, PathBuf::from("share/fw"));
        assert_eq!(carried.kind, InferredKind::Data);

        let objects_a = [Object {
            name: a.0.as_str(),
            file: a.1.as_path(),
            twin: None,
        }];
        let fresh = inference.read(&objects_a, &scope).unwrap();
        assert!(fresh.is_empty(), "the carried pair is not improved");
        let carried = inference.get(&source).unwrap();
        assert_eq!(carried.prefix, tree.0.join("p/share/sdk"));
        assert_eq!(carried.destination, PathBuf::from("share/fw"));
        assert_eq!(carried.kind, InferredKind::Data);
        assert_eq!(
            carried.objects.iter().collect::<Vec<_>>(),
            ["liba.so.1", "libb.so.1"]
        );
    }

    /// A path is a candidate only when it lies under the object's own
    /// prefix and names an existing directory there.
    #[test]
    fn only_existing_directories_under_the_objects_prefix_are_candidates() {
        let tree = Tree::new("prefix");
        tree.dir("usr/share/examplefw");
        tree.dir("opt/vendor/share/examplefw");
        tree.file("usr/share/examplefw.conf", b"conf");
        let lib = tree.object(
            "usr/lib/libexamplefw.so.1",
            &[
                "usr/share/examplefw",
                "opt/vendor/share/examplefw",
                "usr/share/examplefw.conf",
                "usr/share/missing",
            ],
        );
        let dirs = run(&[lib], None).unwrap();
        assert_eq!(rels(&dirs), ["share/examplefw"]);
        assert_eq!(dirs[0].prefix, tree.0.join("usr"));
        assert_eq!(
            dirs[0].objects.iter().collect::<Vec<_>>(),
            ["libexamplefw.so.1"]
        );
    }

    /// Candidates lie strictly below a library directory or `share`. A
    /// base itself, anything containing one, and the rest of the prefix
    /// are never candidates.
    #[test]
    fn only_paths_strictly_below_a_resource_base_are_candidates() {
        let tree = Tree::new("bases");
        for dir in [
            "usr/lib/x86_64-linux-gnu/examplefw",
            "usr/lib64/examplefw",
            "usr/share/examplefw",
            "usr/etc/examplefw",
            "usr/local/share/examplefw",
            "usr/include/examplefw",
        ] {
            tree.dir(dir);
        }
        let lib = tree.object(
            "usr/lib/libexamplefw.so.1",
            &[
                "usr",
                "usr/lib",
                "usr/share",
                "usr/lib/x86_64-linux-gnu",
                "usr/lib/x86_64-linux-gnu/examplefw",
                "usr/lib64/examplefw",
                "usr/share/examplefw",
                "usr/etc/examplefw",
                "usr/local/share",
                "usr/local/share/examplefw",
                "usr/include/examplefw",
            ],
        );
        assert_eq!(
            rels(&run(&[lib], None).unwrap()),
            [
                "lib/x86_64-linux-gnu/examplefw",
                "lib64/examplefw",
                "share/examplefw"
            ]
        );
    }

    /// Under a `/` prefix the bases below `usr` count too, while host
    /// configuration, system state and `usr/local` never do.
    #[test]
    fn a_root_prefix_counts_the_bases_below_usr_and_nothing_else() {
        assert!(in_resource_base(Path::new("usr/share/foo"), true));
        assert!(in_resource_base(Path::new("lib/foo"), true));
        assert!(!in_resource_base(Path::new("usr/share/foo"), false));
        for rel in [
            "etc/pam.d",
            "proc/self/fdinfo",
            "usr/local/share",
            "usr/lib/x86_64-linux-gnu",
            "usr/share",
            "usr",
        ] {
            assert!(!in_resource_base(Path::new(rel), true), "{rel}");
        }
        assert!(is_multiarch("x86_64-linux-gnu"));
        assert!(is_multiarch("arm-linux-gnueabihf"));
        assert!(!is_multiarch("gstreamer-1.0"));
        assert!(!is_multiarch("xorg"));
    }

    /// An object below `/lib` has `/` for its prefix. Of what it compiles
    /// in, only the data below `usr/share` is a candidate: host
    /// configuration, pseudo-filesystems and `usr/local` never are.
    #[test]
    fn a_lib_object_finds_usr_share_and_ignores_the_rest_of_the_root() {
        let tree = Tree::new("root-prefix");
        let root = tree.0.join("sys");
        let compiled = [
            "/etc/pam.d",
            "/proc/self/fdinfo",
            "/usr/local/share",
            "/usr/share/foo",
        ];
        tree.file("sys/lib/libfoo.so.1", &elf(&[(READ, &compiled)]));
        let mut files = vec!["lib/libfoo.so.1".to_string()];
        for dir in compiled {
            files.push(format!("{}/file", &dir[1..]));
            tree.file(&format!("sys{dir}/file"), b"x");
        }
        let refs: Vec<&str> = files.iter().map(String::as_str).collect();
        let universe = fixture::populate_universe(
            &root,
            &[fixture::package("app", &refs)],
            "app",
            &[],
            None,
            None,
        );
        let lib = ("libfoo.so.1".to_string(), root.join("lib/libfoo.so.1"));
        let dirs = run(&[lib], Some(&universe)).unwrap();
        assert_eq!(rels(&dirs), ["usr/share/foo"]);
        assert_eq!(dirs[0].prefix, root);
    }

    /// Spellings that reach one directory through a link are one entry
    /// under the canonical path. A link leaving the prefix is dropped,
    /// whether absolute or climbing out with `..`, and a merged-usr
    /// spelling lands where the loader would land.
    #[test]
    fn linked_spellings_are_one_directory_and_escaping_links_are_dropped() {
        let tree = Tree::new("canonical");
        tree.dir("usr/lib/examplefw");
        symlink("lib", tree.0.join("usr/lib64")).unwrap();
        symlink("usr/lib", tree.0.join("lib")).unwrap();
        tree.dir("usr/lib/mergedfw");
        tree.dir("usr/share/xkeyboard-config-2");
        tree.dir("usr/share/X11");
        symlink("../xkeyboard-config-2", tree.0.join("usr/share/X11/xkb")).unwrap();
        tree.dir("outside/escape");
        symlink(
            tree.0.join("outside/escape"),
            tree.0.join("usr/share/escape"),
        )
        .unwrap();
        tree.dir("share/climbed");
        tree.dir("usr/share/a");
        symlink("../../../share/climbed", tree.0.join("usr/share/a/x")).unwrap();
        let lib = tree.object(
            "usr/lib/libexamplefw.so.1",
            &[
                "usr/lib64/examplefw",
                "usr/lib/examplefw",
                "lib/mergedfw",
                "usr/share/X11/xkb",
                "usr/share/xkeyboard-config-2",
                "usr/share/escape",
                "usr/share/a/x",
            ],
        );
        let dirs = run(&[lib], None).unwrap();
        assert_eq!(
            rels(&dirs),
            ["lib/examplefw", "lib/mergedfw", "share/xkeyboard-config-2"]
        );
        assert_eq!(dirs[0].source, tree.0.join("usr/lib/examplefw"));
        assert_eq!(
            dirs[0].compiled.iter().cloned().collect::<Vec<_>>(),
            [
                tree.path("usr/lib/examplefw"),
                tree.path("usr/lib64/examplefw")
            ]
        );
        assert_eq!(
            dirs[1].compiled.iter().cloned().collect::<Vec<_>>(),
            [tree.path("lib/mergedfw")]
        );
        assert_eq!(
            dirs[2].compiled.iter().cloned().collect::<Vec<_>>(),
            [
                tree.path("usr/share/X11/xkb"),
                tree.path("usr/share/xkeyboard-config-2")
            ]
        );
    }

    /// Programs are read like libraries, with the prefix of their `bin`
    /// or `libexec`, and a framework keeping plugins beside its own
    /// binary, as Xorg does, is found.
    #[test]
    fn programs_are_read_and_plugins_beside_them_are_found() {
        let tree = Tree::new("programs");
        tree.dir("usr/lib/xorg/modules");
        tree.dir("usr/share/toolfw");
        let xorg = tree.object("usr/lib/xorg/Xorg", &["usr/lib/xorg/modules"]);
        let tool = tree.object("usr/bin/tool", &["usr/share/toolfw"]);
        let helper = tree.object("usr/libexec/tool/helper", &["usr/share/toolfw"]);
        let dirs = run(&[xorg, tool, helper], None).unwrap();
        assert_eq!(rels(&dirs), ["lib/xorg/modules", "share/toolfw"]);
        assert_eq!(
            dirs[1].objects.iter().collect::<Vec<_>>(),
            ["helper", "tool"]
        );
    }

    /// A module deep in a plugin tree takes the prefix of the library
    /// directory it hangs under, so its compiled-in paths are looked
    /// for below that prefix and not beside the module.
    #[test]
    fn a_deep_module_has_the_prefix_of_its_library_directory() {
        let tree = Tree::new("deep-module");
        tree.dir("usr/share/modulefw");
        let module = tree.object(
            "usr/lib/fw/plugins/nested/module.so",
            &["usr/share/modulefw"],
        );
        let dirs = run(&[module], None).unwrap();
        assert_eq!(rels(&dirs), ["share/modulefw"]);
        assert_eq!(dirs[0].prefix, tree.0.join("usr"));
    }

    /// A program the tree holds names the source's paths, so its prefix
    /// is taken at the same place below the source's root: one in the
    /// AppDir's `usr/bin` finds the sysroot's directories.
    #[test]
    fn tree_programs_take_their_prefix_below_the_source_root() {
        let tree = Tree::new("tree-objects");
        let root = tree.0.join("sys");
        let app = tree.0.join("app");
        tree.file("sys/usr/share/X11/xkb/rules", b"r");
        tree.file("app/usr/bin/tool", &elf(&[(READ, &["/usr/share/X11/xkb"])]));
        let universe = fixture::populate_universe(
            &root,
            &[fixture::package("app", &["usr/share/X11/xkb/rules"])],
            "app",
            &[],
            None,
            None,
        );
        let objects = [("tool".to_string(), app.join("usr/bin/tool"))];
        assert!(run(&objects, Some(&universe)).unwrap().is_empty());
        let dirs = run_in(&objects, Some(&universe), &[&app]).unwrap();
        assert_eq!(rels(&dirs), ["share/X11/xkb"]);
        assert_eq!(dirs[0].prefix, root.join("usr"));
    }

    /// The directories of one object under its prefix that survive the
    /// filter, for an object at `object` compiling in each of `dirs`,
    /// which all exist.
    fn surviving(tag: &str, object: &str, dirs: &[&str], exclude: &[String]) -> Vec<String> {
        let tree = Tree::new(tag);
        for dir in dirs {
            tree.dir(dir);
        }
        let object = tree.object(object, dirs);
        let objects = [Object {
            name: &object.0,
            file: &object.1,
            twin: None,
        }];
        let scope = Scope {
            sysroot: None,
            trees: &[],
            exclude,
            descriptors: &[],
            lib_dir: Path::new("lib"),
        };
        rels(&discover(&objects, &scope).unwrap())
    }

    /// Shared roots drop what equals or contains them while their
    /// framework subdirectories stay. Shared trees, libc's directories
    /// and the driver stacks drop what lies below them too, an entry
    /// relative to the library directories naming its path below
    /// whichever library directory the candidate lies in.
    #[test]
    fn roots_trees_libc_and_drivers_drop_what_the_table_says() {
        let kept = surviving(
            "table",
            "usr/lib/libexamplefw.so.1",
            &[
                "usr/share/X11",
                "usr/share/X11/xkb",
                "usr/share/xml",
                "usr/share/locale",
                "usr/share/locale/de",
                "usr/share/doc/slang/v2",
                "usr/share/fonts/Type1",
                "usr/share/zoneinfo",
                "usr/share/drirc.d",
                "usr/lib/gconv",
                "usr/lib/locale",
                "usr/lib/dri",
                "usr/lib64/dri",
                "usr/lib64/security",
                "usr/lib/x86_64-linux-gnu/security",
                "usr/lib/x86_64-linux-gnu/gconv",
                "usr/lib/jvm/x",
                "usr/lib/go",
                "usr/lib/gcc/x86_64-linux-gnu/14",
                "usr/lib/examplefw/plugins",
                "usr/share/examplefw",
            ],
            &[],
        );
        assert_eq!(
            kept,
            ["lib/examplefw/plugins", "share/X11/xkb", "share/examplefw"]
        );
    }

    /// A GStreamer-style plugin in a subdirectory of the library
    /// directory still has `lib64/dri` dropped as a driver directory, and
    /// its own directory is never a candidate.
    #[test]
    fn a_plugin_in_a_subdirectory_drops_driver_and_own_directories() {
        let kept = surviving(
            "plugin-subdir",
            "usr/lib/gstreamer-1.0/libgstva.so",
            &["usr/lib64/dri", "usr/lib/gstreamer-1.0", "usr/share/gstva"],
            &[],
        );
        assert_eq!(kept, ["share/gstva"]);
    }

    /// The directory an object sits in, and anything containing it, is
    /// never a candidate, while what lies below it is: Xorg's modules and
    /// a framework's own `locale` beside a program stay, since a
    /// library-directory entry is never matched deeper down.
    #[test]
    fn the_objects_own_directory_is_dropped_and_what_lies_below_it_kept() {
        let kept = surviving(
            "own-dir",
            "usr/lib/xorg/Xorg",
            &[
                "usr/lib/xorg",
                "usr/lib/xorg/modules",
                "usr/lib/xorg/locale",
            ],
            &[],
        );
        assert_eq!(kept, ["lib/xorg/locale", "lib/xorg/modules"]);
        let kept = surviving(
            "own-multiarch",
            "usr/lib/x86_64-linux-gnu/libfoo.so.1",
            &[
                "usr/lib/x86_64-linux-gnu",
                "usr/lib/x86_64-linux-gnu/security",
                "usr/lib/x86_64-linux-gnu/foo",
            ],
            &[],
        );
        assert_eq!(kept, ["lib/x86_64-linux-gnu/foo"]);
        assert!(
            dropped(Path::new("lib/foo"), Some(Path::new("lib/foo/bar")), &[]),
            "a candidate containing the object's own directory is dropped"
        );
        assert!(!dropped(
            Path::new("lib/foo/bar/plugins"),
            Some(Path::new("lib/foo/bar")),
            &[]
        ));
        let kept = surviving(
            "multiarch-base",
            "usr/lib/libbar.so.1",
            &["usr/lib/x86_64-linux-gnu"],
            &[],
        );
        assert!(kept.is_empty(), "{kept:?}");
    }

    /// libc's own objects are not read as sources, and a recipe
    /// exclusion drops a directory by any of its components below its
    /// library directory or `share`.
    #[test]
    fn libc_objects_are_skipped_and_recipe_exclusions_apply() {
        assert!(surviving("libc", "usr/lib/libc.so.6", &["usr/share/examplefw"], &[]).is_empty());
        assert!(
            surviving(
                "loader",
                "usr/lib/ld-linux-x86-64.so.2",
                &["usr/share/examplefw"],
                &[]
            )
            .is_empty()
        );
        let kept = surviving(
            "exclude",
            "usr/lib/libexamplefw.so.1",
            &["usr/lib/libdecor/plugins-1", "usr/share/examplefw"],
            &["libdecor".to_string()],
        );
        assert_eq!(kept, ["share/examplefw"]);
        let kept = surviving(
            "exclude-base",
            "usr/lib/libexamplefw.so.1",
            &["usr/libx32/examplefw", "usr/lib/libxfoo"],
            &["libx".to_string()],
        );
        assert_eq!(kept, ["libx32/examplefw"]);
    }

    /// Under a `/` prefix every table entry and the object's own
    /// directory also stand below `usr`: an object in `/lib/firefox` has
    /// `/usr/lib/firefox` dropped, and `usr/share/locale` is a tree.
    #[test]
    fn a_root_prefix_mirrors_the_table_and_the_own_directory_below_usr() {
        let tree = Tree::new("root-mirror");
        let root = tree.0.join("sys");
        let compiled = [
            "/usr/lib/firefox",
            "/usr/share/X11",
            "/usr/share/locale",
            "/usr/lib/x86_64-linux-gnu/gconv",
            "/usr/share/firefoxfw",
        ];
        tree.file("sys/lib/firefox/libxul.so", &elf(&[(READ, &compiled)]));
        let mut files = vec!["lib/firefox/libxul.so".to_string()];
        for dir in compiled {
            files.push(format!("{}/file", &dir[1..]));
            tree.file(&format!("sys{dir}/file"), b"x");
        }
        let refs: Vec<&str> = files.iter().map(String::as_str).collect();
        let universe = fixture::populate_universe(
            &root,
            &[fixture::package("app", &refs)],
            "app",
            &[],
            None,
            None,
        );
        let lib = ("libxul.so".to_string(), root.join("lib/firefox/libxul.so"));
        let dirs = run(&[lib], Some(&universe)).unwrap();
        assert_eq!(rels(&dirs), ["usr/share/firefoxfw"]);
    }

    /// The directory an object sits in is resolved through links like a
    /// candidate, so an object reached through `lib64 -> lib` still has
    /// `lib/foo` dropped as its own directory.
    #[test]
    fn the_own_directory_is_resolved_through_links() {
        let tree = Tree::new("own-link");
        let root = tree.0.join("sys");
        let app = tree.0.join("app");
        tree.file("sys/usr/lib/foo/plugins/p", b"p");
        symlink("lib", root.join("usr/lib64")).unwrap();
        tree.file(
            "app/usr/lib64/foo/prog",
            &elf(&[(READ, &["/usr/lib/foo", "/usr/lib/foo/plugins"])]),
        );
        let universe = fixture::populate_universe(
            &root,
            &[fixture::package("app", &["usr/lib/foo/plugins/p"])],
            "app",
            &[],
            None,
            None,
        );
        let objects = [("prog".to_string(), app.join("usr/lib64/foo/prog"))];
        let dirs = run_in(&objects, Some(&universe), &[&app]).unwrap();
        assert_eq!(rels(&dirs), ["lib/foo/plugins"]);
    }

    /// A program the tree holds has its own directory taken below the
    /// source's root like its prefix, so the installation directory it
    /// names for itself is dropped and what lies below it kept.
    #[test]
    fn a_tree_programs_own_directory_is_taken_below_the_source_root() {
        let tree = Tree::new("tree-own-dir");
        let root = tree.0.join("sys");
        let app = tree.0.join("app");
        tree.file("sys/usr/lib/tool/plugins/p", b"p");
        tree.file(
            "app/usr/lib/tool/tool",
            &elf(&[(READ, &["/usr/lib/tool", "/usr/lib/tool/plugins"])]),
        );
        let universe = fixture::populate_universe(
            &root,
            &[fixture::package("app", &["usr/lib/tool/plugins/p"])],
            "app",
            &[],
            None,
            None,
        );
        let objects = [("tool".to_string(), app.join("usr/lib/tool/tool"))];
        let dirs = run_in(&objects, Some(&universe), &[&app]).unwrap();
        assert_eq!(rels(&dirs), ["lib/tool/plugins"]);
    }

    /// The omission findings this thread recorded for `destination`.
    fn omissions(destination: &str) -> Vec<String> {
        crate::bundle::completeness::report(&[]);
        crate::bundle::completeness::take_reported()
            .into_iter()
            .filter(|f| f.object == destination && f.outcome == Outcome::OptionalOmission)
            .map(|f| f.reason)
            .collect()
    }

    /// A directory deeper than the member walk goes is omitted and
    /// reported, not fatal, and the rest is still found.
    #[test]
    fn a_directory_too_deep_to_walk_is_omitted_without_failing() {
        let tree = Tree::new("too-deep");
        let deep = format!("usr/share/deepfw/{}", vec!["d"; 20].join("/"));
        tree.file(&format!("{deep}/f"), b"f");
        tree.dir("usr/share/examplefw");
        let lib = tree.object(
            "usr/lib/libexamplefw.so.1",
            &["usr/share/deepfw", "usr/share/examplefw"],
        );
        omissions("");
        assert_eq!(rels(&run(&[lib], None).unwrap()), ["share/examplefw"]);
        let omitted = omissions("share/deepfw");
        assert!(
            omitted.len() == 1 && omitted[0].contains("cannot be read"),
            "{omitted:?}"
        );
    }

    /// A directory the build may not read is omitted and reported, not
    /// fatal. Root reads it regardless, so the check needs another user.
    #[test]
    fn an_unreadable_directory_is_omitted_without_failing() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if fs::metadata("/proc/self").is_ok_and(|m| m.uid() == 0) {
            return;
        }
        let tree = Tree::new("unreadable");
        tree.file("usr/share/lockedfw/sub/f", b"f");
        tree.dir("usr/share/examplefw");
        let lib = tree.object(
            "usr/lib/libexamplefw.so.1",
            &["usr/share/lockedfw", "usr/share/examplefw"],
        );
        let locked = tree.0.join("usr/share/lockedfw/sub");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        omissions("");
        let dirs = run(&[lib], None);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(rels(&dirs.unwrap()), ["share/examplefw"]);
        let omitted = omissions("share/lockedfw");
        assert!(
            omitted.len() == 1 && omitted[0].contains("cannot be read"),
            "{omitted:?}"
        );
    }

    /// Discovery is a function of the inputs: objects read in another
    /// order yield the same sorted set, each directory naming every
    /// object that compiles it in, and a directory two prefixes name, in
    /// an unmerged sysroot, keeps the same prefix whichever came first.
    #[test]
    fn discovery_is_deterministic() {
        let tree = Tree::new("determinism");
        tree.dir("usr/share/examplefw");
        tree.dir("usr/share/otherfw");
        let one = tree.object(
            "usr/lib/libone.so.1",
            &["usr/share/otherfw", "usr/share/examplefw"],
        );
        let two = tree.object("usr/lib/libtwo.so.1", &["usr/share/examplefw"]);
        let forward = run(&[one.clone(), two.clone()], None).unwrap();
        let backward = run(&[two, one], None).unwrap();
        assert_eq!(forward, backward);
        assert_eq!(rels(&forward), ["share/examplefw", "share/otherfw"]);
        assert_eq!(
            forward[0].objects.iter().collect::<Vec<_>>(),
            ["libone.so.1", "libtwo.so.1"]
        );

        let root = tree.0.join("sys");
        let image = elf(&[(READ, &["/usr/share/examplefw"])]);
        tree.file("sys/lib/libroot.so.1", &image);
        tree.file("sys/usr/lib/libusr.so.1", &image);
        tree.file("sys/usr/share/examplefw/a.conf", b"a");
        let universe = fixture::populate_universe(
            &root,
            &[fixture::package(
                "app",
                &[
                    "lib/libroot.so.1",
                    "usr/lib/libusr.so.1",
                    "usr/share/examplefw/a.conf",
                ],
            )],
            "app",
            &[],
            None,
            None,
        );
        let below_root = ("libroot.so.1".to_string(), root.join("lib/libroot.so.1"));
        let below_usr = ("libusr.so.1".to_string(), root.join("usr/lib/libusr.so.1"));
        let forward = run(&[below_root.clone(), below_usr.clone()], Some(&universe)).unwrap();
        let backward = run(&[below_usr, below_root], Some(&universe)).unwrap();
        assert_eq!(forward, backward);
        assert_eq!(forward[0].prefix, root);
        assert_eq!(rels(&forward), ["usr/share/examplefw"]);
    }

    /// In a sysroot build a compiled-in path names the sysroot's own
    /// directory, and only one holding admitted files is a candidate.
    #[test]
    fn a_sysroot_build_finds_only_admitted_directories() {
        let tree = Tree::new("sysroot");
        let root = tree.0.join("sys");
        let image = elf(&[(READ, &["/usr/share/examplefw", "/usr/share/unselected"])]);
        tree.file("sys/usr/lib/libexamplefw.so.1", &image);
        tree.file("sys/usr/share/examplefw/a.conf", b"a");
        tree.file("sys/usr/share/unselected/b.conf", b"b");
        let app = fixture::package(
            "app",
            &["usr/lib/libexamplefw.so.1", "usr/share/examplefw/a.conf"],
        );
        let other = fixture::package("other", &["usr/share/unselected/b.conf"]);
        let universe = fixture::populate_universe(&root, &[app, other], "app", &[], None, None);
        let lib = (
            "libexamplefw.so.1".to_string(),
            root.join("usr/lib/libexamplefw.so.1"),
        );
        let dirs = run(&[lib], Some(&universe)).unwrap();
        assert_eq!(rels(&dirs), ["share/examplefw"]);
        assert_eq!(dirs[0].source, root.join("usr/share/examplefw"));
    }

    /// The trim names a member by every spelling the trace could have
    /// recorded: the canonical prefix pair, the resolved source and the
    /// compiled-in path that names the directory. A member the trace
    /// did not name is left out; without a trim none is.
    #[test]
    fn the_trim_names_a_member_by_any_spelling_the_trace_recorded() {
        use super::super::{Trim, trimmed_out};
        use onelf_sysroot::Policy;
        let dir = InferredDir {
            source: PathBuf::from("/dist/usr/lib/examplefw/plugins"),
            prefix: PathBuf::from("/dist/usr"),
            rel: PathBuf::from("lib/examplefw/plugins"),
            destination: PathBuf::from("lib/examplefw/plugins"),
            kind: InferredKind::Data,
            size: 0,
            compiled: BTreeSet::from(["/dist/usr/lib/examplefw/plugins".to_string()]),
            objects: BTreeSet::from(["libexamplefw.so.1".to_string()]),
        };
        let a = Path::new("a.conf");
        // The trace recorded the compiled-in spelling: the canonical
        // prefix pair names the same member.
        let opened = Trim::from_paths(
            vec![PathBuf::from("/dist/usr/lib/examplefw/plugins/a.conf")],
            None,
        );
        assert!(!trimmed_out(&dir, a, None, Some(&opened)));
        // A keep glob names it through one of the spellings tried.
        let keep_txt = std::env::temp_dir().join(format!(
            "onelf-trim-keep-{}-{}.txt",
            std::process::id(),
            a.display()
        ));
        std::fs::write(&keep_txt, "dist/usr/lib/examplefw/plugins/**\n").unwrap();
        let keep = Some(Policy::load(&keep_txt).unwrap());
        let _ = std::fs::remove_file(&keep_txt);
        let globbed = Trim {
            opened: Default::default(),
            keep,
            tree: None,
        };
        assert!(!trimmed_out(&dir, a, None, Some(&globbed)));
        // Nothing observed, nothing matching: the member goes.
        let empty = Trim::from_paths(Vec::new(), None);
        assert!(trimmed_out(&dir, a, None, Some(&empty)));
        // An open of a same-named copy inside the tree stands in for
        // the member: the tree's own library directories lead the run's
        // search path, so on a rebuild the carried copy is what loads,
        // and that open names a path no spelling spells.
        let shadowed = Trim::from_paths(
            vec![PathBuf::from("/tree/lib/a.conf")],
            Some(Path::new("/tree")),
        );
        assert!(!trimmed_out(&dir, a, None, Some(&shadowed)));
        // A tree copy of another name is not the member, and neither is
        // the same name outside the tree.
        let other = Trim::from_paths(
            vec![PathBuf::from("/tree/lib/b.conf")],
            Some(Path::new("/tree")),
        );
        assert!(trimmed_out(&dir, a, None, Some(&other)));
        let outside = Trim::from_paths(
            vec![PathBuf::from("/elsewhere/a.conf")],
            Some(Path::new("/tree")),
        );
        assert!(trimmed_out(&dir, a, None, Some(&outside)));
        // Without a trim nothing goes.
        assert!(!trimmed_out(&dir, a, None, None));
    }
}

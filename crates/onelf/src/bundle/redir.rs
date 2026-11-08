//! The build-time redirection choice for every carried inferred directory.
//!
//! Each directory a library compiled in reaches the application at launch
//! by exactly one mechanism, decided here when the package is built: a
//! descriptor's runtime mapping that already names the directory, else a
//! descriptor anchor path the compiled-in strings are long enough to
//! carry, else a path pair for a namespace overlay. The choice is a pure
//! function of the carried directories, the descriptors and the assembled
//! tree, so fixed inputs give fixed output.
//!
//! A directory carried inside another carried one takes its parent's
//! mechanism. The overlay binds the parent's tree path, so spellings
//! beneath it resolve; the anchor record exposes everything under the
//! anchored directory; and an environment mapping covers a lookup that
//! reaches the child through the mapped variable. A child spelling that
//! names the parent through a different alias, or names the child by its
//! own absolute path while only the variable redirects the parent, is
//! not redirected by the parent's choice; the anchor record writer and
//! the compiled-in rewrite decide those per spelling. Only top-level
//! carried directories are decided here.
//!
//! The record a previous build left (`.onelf/anchors`) folds into the
//! decision, so a re-run keeps reporting and recording the anchors its
//! bytes already name; `merge` holds those rules.

use std::fs;
use std::io;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::infer::InferredDir;
use onelf_format::anchors;
use onelf_format::rules::{Descriptor, PathSelection, ResourceKind, RuntimeBase, RuntimePathGroup};

/// How one carried inferred directory reaches the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mechanism {
    /// A descriptor's runtime mapping names the directory, so the launch
    /// sets its environment variable to the bundled copy and the
    /// compiled-in strings stay as they are.
    Env,
    /// Every compiled-in string is rewritten to the anchor path.
    Anchor,
    /// The compiled-in strings are too short for an anchor path: a path
    /// pair redirects a launch that has one, and an unredirected launch
    /// is reported.
    Pair,
}

/// One carried inferred directory's choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Redirection {
    /// The bundle directory, relative to the package root.
    pub dest: PathBuf,
    /// The mechanism chosen for it.
    pub mechanism: Mechanism,
    /// The anchor key, for an anchor.
    pub key: Option<String>,
}

/// Decide the mechanism for every carried directory.
///
/// `lib_dirs` is the library directory list the launch evaluates
/// library-root candidates against, relative to the package root: the
/// same list the packer writes into the manifest, which the runtime
/// joins under the package root for its own scope (`onelf-rt`'s
/// `view_env_assignments`). Sets and prepared subtrees answer later
/// scopes and are skipped, since the application package alone carries
/// inferred directories and an application scope match outranks theirs.
pub(crate) fn choose(
    dirs: &[&InferredDir],
    descriptors: &[&Descriptor],
    lib_dirs: &[String],
    directory: &Path,
) -> Vec<Redirection> {
    let mut out: Vec<Redirection> = dirs
        .iter()
        .filter(|dir| env_covered(dir, descriptors, lib_dirs, directory))
        .map(|dir| Redirection {
            dest: dir.destination.clone(),
            mechanism: Mechanism::Env,
            key: None,
        })
        .collect();
    // The rest compete for anchors: keys go to the destinations in sorted
    // order, shortest first. A destination that is not UTF-8 cannot enter
    // the anchor record, so it takes the overlay. A directory keeps its
    // anchor only while every spelling it compiles in is long enough to
    // hold the provisional anchor path. Filtering can only shrink the set,
    // and the keys are assigned in the same sorted order, so the final
    // keys are never longer than the provisional ones the check used.
    let (utf8, mut pairs): (Vec<&InferredDir>, Vec<&InferredDir>) = dirs
        .iter()
        .filter(|dir| !env_covered(dir, descriptors, lib_dirs, directory))
        .partition(|dir| dir.destination.to_str().is_some());
    let mut rest = utf8;
    rest.sort_by(|a, b| a.destination.cmp(&b.destination));
    let Ok(provisional) = anchors::assign(
        rest.iter()
            .map(|dir| dir.destination.to_string_lossy().into_owned()),
    ) else {
        // More destinations than the record holds, or a malformed one:
        // none of them is anchored, and every one takes the overlay.
        pairs.extend(rest);
        return sort_by_dest(finish(out, &pairs));
    };
    let anchored: Vec<&InferredDir> = rest
        .iter()
        .filter(|dir| {
            let anchor = provisional
                .iter()
                .find(|anchor| Path::new(&anchor.relative) == dir.destination)
                .expect("assign returns every destination");
            dir.compiled.iter().all(|s| s.len() >= anchor.path().len())
        })
        .copied()
        .collect();
    let unanchored = |dir: &InferredDir| {
        !anchored
            .iter()
            .any(|kept| kept.destination == dir.destination)
    };
    let Ok(final_anchors) = anchors::assign(
        anchored
            .iter()
            .map(|dir| dir.destination.to_string_lossy().into_owned()),
    ) else {
        // The kept set no longer fits the record: every one of them takes
        // the overlay, and the rest keeps its own.
        pairs.extend(anchored.iter().copied());
        pairs.extend(rest.iter().copied().filter(|dir| unanchored(dir)));
        return sort_by_dest(finish(out, &pairs));
    };
    for dir in &anchored {
        let anchor = final_anchors
            .iter()
            .find(|anchor| Path::new(&anchor.relative) == dir.destination)
            .expect("assign returns every destination");
        out.push(Redirection {
            dest: dir.destination.clone(),
            mechanism: Mechanism::Anchor,
            key: Some(anchor.key.clone()),
        });
    }
    for dir in rest.iter().copied().filter(|dir| unanchored(dir)) {
        pairs.push(dir);
    }
    sort_by_dest(finish(out, &pairs))
}

/// The pair redirection for every directory left without a mechanism.
fn finish(mut out: Vec<Redirection>, pairs: &[&InferredDir]) -> Vec<Redirection> {
    for dir in pairs {
        out.push(Redirection {
            dest: dir.destination.clone(),
            mechanism: Mechanism::Pair,
            key: None,
        });
    }
    out
}

/// Report the choice per carried directory.
pub(crate) fn report(redirections: &[Redirection]) {
    if redirections.is_empty() {
        return;
    }
    eprintln!("  Redirected inferred directories:");
    for r in redirections {
        let detail = match (&r.mechanism, &r.key) {
            (Mechanism::Env, _) => "environment mapping".to_string(),
            (Mechanism::Anchor, Some(key)) => format!("anchor {key}"),
            _ => "path pair".to_string(),
        };
        eprintln!("    {}: {}", r.dest.display(), detail);
    }
}

impl Redirection {
    /// Whether the mechanism still names something: a carried directory
    /// the drop pass emptied is gone from the tree, and no mechanism
    /// reaches through an absent copy.
    pub(crate) fn resolves(&self, directory: &Path) -> bool {
        directory.join(&self.dest).is_dir()
    }
}

/// The anchors a tree already carries, from a previous build. Empty
/// when the tree holds no record.
pub(crate) fn persisted(directory: &Path) -> io::Result<Vec<anchors::Anchor>> {
    let path = directory.join(anchors::ANCHORS_FILE);
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    // A bound before the allocation: decode names an oversized record,
    // and the read never holds more than one byte past its limit.
    let mut bytes = Vec::new();
    (&file)
        .take(anchors::MAX_RECORD_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    anchors::decode(&bytes).map_err(io::Error::other)
}

/// Fold the record a previous build left into the fresh decision.
///
/// A destination the record anchors stays anchored whatever the fresh
/// decision says: the tree's copies already name its key, and no other
/// mechanism reaches through a rewritten string. A destination the drop
/// pass emptied leaves. The record's keys are positional over its
/// sorted entries, so the merged anchor list takes canonical keys from
/// its own order, and an anchor whose key the merge would move stops
/// the build: the existing rewrites name the old key, and a moved key
/// would resolve them to the wrong directory. A fresh tree bundles
/// every directory whole and is the way out.
pub(crate) fn merge(
    fresh: Vec<Redirection>,
    persisted: &[anchors::Anchor],
    directory: &Path,
) -> io::Result<Vec<Redirection>> {
    if persisted.is_empty() {
        return Ok(fresh);
    }
    // Every destination the record still anchors, plus every fresh
    // anchor, sorted the way the record orders its entries. A fresh
    // decision for a destination the record anchors loses: the rewrite
    // is already in the tree's bytes.
    let mut candidates: Vec<String> = persisted
        .iter()
        .filter(|anchor| directory.join(&anchor.relative).is_dir())
        .map(|anchor| anchor.relative.clone())
        .collect();
    candidates.extend(
        fresh
            .iter()
            .filter(|r| r.mechanism == Mechanism::Anchor && !persisted_anchored(persisted, &r.dest))
            .map(|r| r.dest.to_string_lossy().into_owned()),
    );
    candidates.sort();
    candidates.dedup();
    let assigned = anchors::assign(candidates).map_err(io::Error::other)?;
    for anchor in persisted {
        let Some(merged) = assigned
            .iter()
            .find(|merged| merged.relative == anchor.relative)
        else {
            continue;
        };
        if merged.key != anchor.key {
            return Err(io::Error::other(format!(
                "anchor {} names {} in the tree's record but the merged anchors assign it to {}; \
                 bundle a fresh tree",
                anchor.key, anchor.relative, merged.key,
            )));
        }
    }
    let mut merged: Vec<Redirection> = assigned
        .iter()
        .map(|anchor| Redirection {
            dest: PathBuf::from(&anchor.relative),
            mechanism: Mechanism::Anchor,
            key: Some(anchor.key.clone()),
        })
        .collect();
    merged.extend(
        fresh.into_iter().filter(|r| {
            r.mechanism != Mechanism::Anchor && !persisted_anchored(persisted, &r.dest)
        }),
    );
    merged.sort_by(|a, b| a.dest.cmp(&b.dest));
    Ok(merged)
}

/// Whether the record anchors `dest`.
fn persisted_anchored(persisted: &[anchors::Anchor], dest: &Path) -> bool {
    persisted
        .iter()
        .any(|anchor| Path::new(&anchor.relative) == dest)
}

/// Rewrite every exact occurrence of an anchored directory's compiled-in
/// spellings in the bundled copies to the anchor path, padded with NUL
/// bytes to the original length. Occurrences are whole NUL-terminated
/// strings in loadable read-only data, `.dynstr` excluded, so a longer
/// path that shares the prefix without a separator is untouched.
///
/// Returns the number of files changed and the number of occurrences
/// rewritten. The bytes land before the ledger attests them, so the
/// recorded hashes are the rewritten ones.
pub(crate) fn rewrite_anchors(
    redirections: &[Redirection],
    dirs: &[&InferredDir],
    files: &[PathBuf],
) -> io::Result<(usize, usize)> {
    let mut patterns: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for r in redirections {
        let (Mechanism::Anchor, Some(key)) = (&r.mechanism, &r.key) else {
            continue;
        };
        let Some(dir) = dirs.iter().find(|dir| dir.destination == r.dest) else {
            continue;
        };
        let anchor = anchors::Anchor {
            key: key.clone(),
            relative: r.dest.to_string_lossy().into_owned(),
        };
        for spelling in &dir.compiled {
            patterns.push(padded_pair(spelling, &anchor, &r.dest)?);
        }
    }
    if patterns.is_empty() {
        return Ok((0, 0));
    }
    patterns.sort();
    patterns.dedup();
    let mut files_changed = 0;
    let mut occurrences = 0;
    for file in files {
        let mut data = fs::read(file)?;
        let ranges = super::infer::read_only_ranges(&data);
        if ranges.is_empty() {
            continue;
        }
        let mut modified = false;
        for (start, end) in ranges {
            for (pattern, replacement) in &patterns {
                let mut from = start;
                while let Some(found) = find(&data[from..end], pattern) {
                    let at = from + found;
                    let preceded = at == start || data[at - 1] == 0;
                    if preceded {
                        data[at..at + pattern.len()].copy_from_slice(replacement);
                        modified = true;
                        occurrences += 1;
                    }
                    from = at + pattern.len();
                    if from + pattern.len() > end {
                        break;
                    }
                }
            }
        }
        if modified {
            // The copy step may have left the file owner-read-only, and
            // a hardlink may share its inode, so the write follows
            // finalize_tree: give the file its own inode, grant the
            // owner write, then re-pin the mtime and restore the mode
            // the copy step left. A link that cannot be detached would
            // carry the write to the file it shares its inode with, so
            // the rewrite refuses rather than proceed.
            super::elf::break_hardlink(file)?;
            let perms = fs::metadata(file)
                .map(|m| m.permissions().mode())
                .unwrap_or(0o755);
            let needs_chmod = perms & 0o200 == 0;
            if needs_chmod {
                let _ = fs::set_permissions(file, PermissionsExt::from_mode(perms | 0o200));
            }
            let written = fs::write(file, &data);
            super::normalize_mtime(file);
            if needs_chmod {
                let _ = fs::set_permissions(file, PermissionsExt::from_mode(perms));
            }
            written?;
            files_changed += 1;
        }
    }
    Ok((files_changed, occurrences))
}

/// The NUL-terminated pattern for `spelling` and its replacement, the
/// anchor path padded with NUL bytes to the pattern's length. The
/// window is the spelling plus its terminator, and the replacement
/// must leave a terminator: a spelling shorter than the anchor path
/// cannot be padded, so the rewrite would run over it.
fn padded_pair(
    spelling: &str,
    anchor: &anchors::Anchor,
    dest: &Path,
) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let replacement = anchor.path().into_bytes();
    let mut pattern = spelling.as_bytes().to_vec();
    pattern.push(0);
    if spelling.len() < replacement.len() {
        return Err(io::Error::other(format!(
            "compiled-in path {spelling} is shorter than its anchor path {} for {}, \
             so it cannot be rewritten; bundle a fresh tree",
            anchor.path(),
            dest.display()
        )));
    }
    let mut bytes = replacement;
    bytes.resize(pattern.len(), 0);
    Ok((pattern, bytes))
}

/// A fresh anchor's spellings must fit the key the merged assignment
/// gave it before the record lands: `choose` checked them against the
/// key the fresh assignment alone produced, and the record's entries
/// sort in ahead of the fresh ones, so the merged key can be longer
/// and the rewrite would refuse a decision already recorded. A
/// destination the record already anchors was rewritten under exactly
/// the key the merge keeps, so it fits by construction.
pub(crate) fn ensure_spellings_fit(
    redirections: &[Redirection],
    persisted: &[anchors::Anchor],
    dirs: &[&InferredDir],
) -> io::Result<()> {
    for r in redirections {
        let (Mechanism::Anchor, Some(key)) = (&r.mechanism, &r.key) else {
            continue;
        };
        if persisted
            .iter()
            .any(|anchor| Path::new(&anchor.relative) == r.dest)
        {
            continue;
        }
        let Some(dir) = dirs.iter().find(|dir| dir.destination == r.dest) else {
            continue;
        };
        let anchor = anchors::Anchor {
            key: key.clone(),
            relative: r.dest.to_string_lossy().into_owned(),
        };
        for spelling in &dir.compiled {
            padded_pair(spelling, &anchor, &r.dest)?;
        }
    }
    Ok(())
}

/// The first offset of `pattern` in `data`, or `None`.
fn find(data: &[u8], pattern: &[u8]) -> Option<usize> {
    if pattern.is_empty() || data.len() < pattern.len() {
        return None;
    }
    (0..=data.len() - pattern.len()).find(|at| &data[*at..at + pattern.len()] == pattern)
}

/// Write `.onelf/anchors` for the anchored redirections whose destination
/// the tree still carries, and record the file in the generated ledger.
/// Returns the number of records written. An empty decision drops a
/// record a previous build left, with its ledger entry.
pub(crate) fn write_record(redirections: &[Redirection], directory: &Path) -> io::Result<usize> {
    let anchored: Vec<anchors::Anchor> = redirections
        .iter()
        .filter(|r| {
            matches!((r.mechanism, &r.key), (Mechanism::Anchor, Some(_))) && r.resolves(directory)
        })
        .map(|r| anchors::Anchor {
            key: r.key.clone().expect("anchored redirection has a key"),
            relative: r.dest.to_string_lossy().into_owned(),
        })
        .collect();
    if anchored.is_empty() {
        // A record a previous build left names directories the tree no
        // longer carries, and the packer refuses such a record, so the
        // empty decision drops the file and its ledger entry.
        let stale = directory.join(anchors::ANCHORS_FILE);
        match fs::remove_file(&stale) {
            Ok(()) => {
                super::sysroot::remove_generated(directory, &[anchors::ANCHORS_FILE.to_string()])?
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        return Ok(0);
    }
    let bytes = anchors::encode(&anchored).map_err(io::Error::other)?;
    let record = directory.join(anchors::ANCHORS_FILE);
    if let Some(parent) = record.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&record, bytes)?;
    super::normalize_mtime(&record);
    super::sysroot::record_generated(directory, vec![anchors::ANCHORS_FILE.to_string()])?;
    Ok(anchored.len())
}

/// Whether a descriptor's runtime mapping already selects the directory's
/// bundle copy, decided the way the runtime evaluates the same groups
/// against the same roots (`onelf-rt`'s `evaluate_path_groups` against
/// the application scope alone): the package root is the assembled tree
/// and the library roots are the manifest's library directories joined
/// under it.
fn env_covered(
    dir: &InferredDir,
    descriptors: &[&Descriptor],
    lib_dirs: &[String],
    directory: &Path,
) -> bool {
    // The launch resolves the variable against the tree root, so the
    // bundle copy it would name is the destination under the tree.
    let dest = directory.join(&dir.destination);
    let lib_roots: Vec<PathBuf> = lib_dirs.iter().map(|d| directory.join(d)).collect();
    descriptors.iter().any(|d| {
        d.runtime
            .paths
            .iter()
            .any(|group| env_selects(group, &lib_roots, directory, &dest))
    })
}

/// Whether the runtime assigns a variable a value naming `dest`, which
/// takes the same list of selected paths the runtime builds and the same
/// representability check on it: a list that cannot join into one value
/// assigns nothing, so it covers nothing. The check runs against a
/// neutral root, since representability is a property of the relative
/// spellings the descriptors and the manifest carry and the launch joins
/// them under its own root, which the build does not know; the check is
/// exact for every colon-free launch root. The runtime joins matches
/// from its later scopes after the application's. The prepared subtrees
/// of selected set pins are packed as the application's own content and
/// are known here, but they are omitted from the mirror deliberately:
/// they answer only after the application scope, and a separator inside
/// one is the same pathology as inside a set tree, so the boundary is
/// stated once for both. Raw set trees are fetched at launch and unseen
/// here.
fn env_selects(
    group: &RuntimePathGroup,
    lib_roots: &[PathBuf],
    directory: &Path,
    dest: &Path,
) -> bool {
    let kind = |path: &Path| match group.kind {
        ResourceKind::Directory => path.is_dir(),
        ResourceKind::File => path.is_file(),
    };
    let roots = |base: RuntimeBase| -> Vec<&Path> {
        match base {
            RuntimeBase::PackageRoot => vec![directory],
            RuntimeBase::LibraryRoot => lib_roots.iter().map(PathBuf::as_path).collect(),
        }
    };
    let chosen: Vec<PathBuf> = match group.selection {
        // Candidates in declaration order, each against its base's roots,
        // first of the declared kind wins.
        PathSelection::First => group
            .candidates
            .iter()
            .flat_map(|candidate| {
                roots(candidate.base)
                    .into_iter()
                    .map(move |root| root.join(candidate.path.as_str()))
            })
            .find(|path| kind(path))
            .into_iter()
            .collect(),
        // Every candidate against every root, in the same order, without
        // duplicates.
        PathSelection::All => {
            let mut found: Vec<PathBuf> = Vec::new();
            for candidate in &group.candidates {
                for root in roots(candidate.base) {
                    let path = root.join(candidate.path.as_str());
                    if kind(&path) && !found.contains(&path) {
                        found.push(path);
                    }
                }
            }
            found
        }
        // Each library root answered by its first matching candidate, the
        // way the runtime skips a root whose matches are all duplicates:
        // the root keeps looking, and a duplicate contributes nothing.
        PathSelection::FirstPerLibraryRoot => {
            let mut found: Vec<PathBuf> = Vec::new();
            for root in lib_roots {
                for candidate in &group.candidates {
                    let path = root.join(candidate.path.as_str());
                    if kind(&path) && !found.contains(&path) {
                        found.push(path);
                        break;
                    }
                }
            }
            found
        }
    };
    !chosen.is_empty()
        && chosen.iter().any(|path| path == dest)
        && std::env::join_paths(
            chosen
                .iter()
                .map(|path| Path::new("/pkg").join(path.strip_prefix(directory).unwrap_or(path))),
        )
        .is_ok()
}

fn sort_by_dest(mut out: Vec<Redirection>) -> Vec<Redirection> {
    out.sort_by(|a, b| a.dest.cmp(&b.dest));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::infer::{InferredDir, InferredKind};
    use onelf_format::rules::{
        BuildRules, EnvironmentName, Inheritance, RelativePath, Resources, RuleId,
        RuntimeCandidate, RuntimeRules, SCHEMA_VERSION,
    };
    use std::collections::BTreeSet;
    use std::fs;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("onelf-redir-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("temp root");
        root
    }

    fn descriptor(id: &str, paths: Vec<RuntimePathGroup>) -> Descriptor {
        Descriptor {
            schema: SCHEMA_VERSION,
            id: RuleId::new(id).unwrap(),
            triggers: Vec::new(),
            resources: Resources::default(),
            build: BuildRules::default(),
            runtime: RuntimeRules {
                paths,
                hooks: Vec::new(),
            },
        }
    }

    fn group(
        id: &str,
        base: RuntimeBase,
        path: &str,
        selection: PathSelection,
    ) -> RuntimePathGroup {
        RuntimePathGroup {
            id: RuleId::new(id).unwrap(),
            kind: ResourceKind::Directory,
            candidates: vec![RuntimeCandidate {
                base,
                path: RelativePath::new(path).unwrap(),
            }],
            variables: vec![EnvironmentName::new("PROOF_DIR").unwrap()],
            selection,
            inheritance: Inheritance::Replace,
        }
    }

    fn candidates(base: RuntimeBase, paths: &[&str]) -> Vec<RuntimeCandidate> {
        paths
            .iter()
            .map(|path| RuntimeCandidate {
                base,
                path: RelativePath::new(path).unwrap(),
            })
            .collect()
    }

    /// A carried directory at `dest`, with `compiled` as its sorted
    /// compiled-in spellings, materialized in the tree so the runtime
    /// selection's on-disk kind checks see it.
    fn dir(root: &Path, dest: &str, compiled: &[&str]) -> InferredDir {
        fs::create_dir_all(root.join(dest)).expect("dest directory");
        InferredDir {
            source: root.join("src").join(dest),
            prefix: root.join("src"),
            rel: PathBuf::from(dest),
            destination: PathBuf::from(dest),
            kind: InferredKind::Data,
            size: 0,
            compiled: compiled.iter().map(|s| s.to_string()).collect(),
            objects: BTreeSet::new(),
        }
    }

    /// A mechanism that already names the directory wins over an anchor,
    /// even when every string is long enough to carry one.
    #[test]
    fn env_mapping_wins_before_anchor() {
        let root = temp_root("redir-env-first");
        let dirs = [dir(
            &root,
            "etc/foo",
            &["etc/fooaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"],
        )];
        let refs: Vec<&InferredDir> = dirs.iter().collect();
        let rules = [descriptor(
            "proof",
            vec![group(
                "proof-g",
                RuntimeBase::PackageRoot,
                "etc/foo",
                PathSelection::First,
            )],
        )];
        let refs_rules: Vec<&Descriptor> = rules.iter().collect();
        let out = choose(&refs, &refs_rules, &["lib".to_string()], &root);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].mechanism, Mechanism::Env);
        assert_eq!(out[0].key, None);
    }

    /// A directory the descriptors never name anchors when its compiled
    /// spellings are long enough to hold the anchor path.
    #[test]
    fn anchor_when_strings_carry_the_path() {
        let root = temp_root("redir-anchor");
        let dirs = [dir(
            &root,
            "lib/appdir",
            &["lib/appdiraaaaaaaaaaaaaaaaaaaa"],
        )];
        let refs: Vec<&InferredDir> = dirs.iter().collect();
        let out = choose(&refs, &[], &["lib".to_string()], &root);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].mechanism, Mechanism::Anchor);
        assert!(out[0].key.is_some());
    }

    /// A directory whose only compiled spelling is too short for the
    /// anchor path takes the overlay path pair.
    #[test]
    fn pair_when_strings_are_short() {
        let root = temp_root("redir-pair");
        let dirs = [dir(&root, "lib/s", &["lib/s"])];
        let refs: Vec<&InferredDir> = dirs.iter().collect();
        let out = choose(&refs, &[], &["lib".to_string()], &root);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].mechanism, Mechanism::Pair);
        assert_eq!(out[0].key, None);
    }

    /// Anchors go to the destinations in sorted order: the earlier
    /// destination keeps the short key, and one long-enough competitor
    /// stays anchored while a short one beside it falls to the pair, so
    /// the short key is never wasted on a directory that cannot use it.
    #[test]
    fn short_competitor_does_not_waste_a_short_key() {
        let root = temp_root("redir-keep");
        let mut dirs = [
            dir(&root, "lib/b", &["lib/b"]),
            dir(&root, "lib/aaaaaaaaaa", &["lib/aaaaaaaaaaaaaaaaaaaa"]),
        ];
        dirs.sort_by(|a, b| a.destination.cmp(&b.destination));
        let refs: Vec<&InferredDir> = dirs.iter().collect();
        let out = choose(&refs, &[], &["lib".to_string()], &root);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].dest, Path::new("lib/aaaaaaaaaa"));
        assert_eq!(out[0].mechanism, Mechanism::Anchor);
        assert_eq!(out[0].key.as_deref(), Some("a"));
        assert_eq!(out[1].dest, Path::new("lib/b"));
        assert_eq!(out[1].mechanism, Mechanism::Pair);
    }

    /// The build directory's own spelling does not flip the decision:
    /// representability is a property of the relative spellings the
    /// descriptors and the manifest carry, and the launch joins them
    /// under its own root, so a separator in the build path changes
    /// nothing while a separator in a selected relative path still
    /// drops the mapping.
    #[test]
    fn build_root_spelling_does_not_affect_representability() {
        let root = temp_root("redir:colon-buildroot");
        let dirs = [dir(
            &root,
            "share/fw",
            &["share/fwaaaaaaaaaaaaaaaaaaaaaaaaa"],
        )];
        let refs: Vec<&InferredDir> = dirs.iter().collect();
        let rules = [descriptor(
            "proof",
            vec![group(
                "proof-g",
                RuntimeBase::PackageRoot,
                "share/fw",
                PathSelection::First,
            )],
        )];
        let refs_rules: Vec<&Descriptor> = rules.iter().collect();
        let out = choose(&refs, &refs_rules, &["lib".to_string()], &root);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].mechanism, Mechanism::Env);
    }

    /// A library root whose matches are all duplicates contributes
    /// nothing and keeps looking, the way the runtime does: the later
    /// candidate it then reaches joins the selected list, and a
    /// separator in that candidate's path still drops the whole
    /// assignment.
    #[test]
    fn duplicate_root_matches_do_not_shorten_the_selection() {
        let root = temp_root("redir-fplr-dup");
        fs::create_dir_all(root.join("lib/a/fw")).expect("first match");
        fs::create_dir_all(root.join("lib/a/ba:d")).expect("separator match");
        let dirs = [dir(
            &root,
            "lib/a/fw",
            &["lib/a/fwaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"],
        )];
        let refs: Vec<&InferredDir> = dirs.iter().collect();
        let mut rules = descriptor("proof", Vec::new());
        rules.runtime.paths = vec![RuntimePathGroup {
            id: RuleId::new("proof-g").unwrap(),
            kind: ResourceKind::Directory,
            candidates: candidates(RuntimeBase::LibraryRoot, &["fw", "a/fw", "ba:d"]),
            variables: vec![EnvironmentName::new("PROOF_DIR").unwrap()],
            selection: PathSelection::FirstPerLibraryRoot,
            inheritance: Inheritance::Replace,
        }];
        let refs_rules = vec![&rules];
        // The second library root sees its own first candidate as a
        // duplicate and moves on to the separator path, which the launch
        // cannot join into a value, so nothing is assigned.
        let out = choose(
            &refs,
            &refs_rules,
            &["lib".to_string(), "lib/a".to_string()],
            &root,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].mechanism, Mechanism::Anchor);
    }

    /// A `first` selection names the directory only when its first
    /// existing candidate is the directory itself: an earlier candidate
    /// that exists in the tree answers the launch and the directory is
    /// not covered.
    #[test]
    fn first_selection_requires_the_first_existing_candidate() {
        let root = temp_root("redir-first");
        fs::create_dir_all(root.join("etc/absent")).expect("earlier candidate");
        let dirs = [dir(
            &root,
            "etc/foo",
            &["etc/fooaaaaaaaaaaaaaaaaaaaaaaaaaaa"],
        )];
        let refs: Vec<&InferredDir> = dirs.iter().collect();
        let mut rules = descriptor("proof", Vec::new());
        rules.runtime.paths = vec![RuntimePathGroup {
            id: RuleId::new("proof-g").unwrap(),
            kind: ResourceKind::Directory,
            candidates: candidates(RuntimeBase::PackageRoot, &["etc/absent", "etc/foo"]),
            variables: vec![EnvironmentName::new("PROOF_DIR").unwrap()],
            selection: PathSelection::First,
            inheritance: Inheritance::Replace,
        }];
        let refs_rules = [&rules];
        let out = choose(&refs, &refs_rules, &["lib".to_string()], &root);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].mechanism, Mechanism::Anchor);
    }

    /// An `all` selection names the directory when any candidate resolves
    /// to it, even behind a first candidate that does not exist.
    #[test]
    fn all_selection_when_any_candidate_matches() {
        let root = temp_root("redir-all");
        let dirs = [dir(
            &root,
            "etc/foo",
            &["etc/fooaaaaaaaaaaaaaaaaaaaaaaaaaaa"],
        )];
        let refs: Vec<&InferredDir> = dirs.iter().collect();
        let mut rules = descriptor("proof", Vec::new());
        rules.runtime.paths = vec![RuntimePathGroup {
            id: RuleId::new("proof-g").unwrap(),
            kind: ResourceKind::Directory,
            candidates: candidates(RuntimeBase::PackageRoot, &["etc/absent", "etc/foo"]),
            variables: vec![EnvironmentName::new("PROOF_DIR").unwrap()],
            selection: PathSelection::All,
            inheritance: Inheritance::Replace,
        }];
        let refs_rules = [&rules];
        let out = choose(&refs, &refs_rules, &["lib".to_string()], &root);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].mechanism, Mechanism::Env);
    }

    /// A per-library-root selection evaluates its candidates against the
    /// bundle's library root, mirroring the runtime's own evaluation for
    /// the build's single library root.
    #[test]
    fn first_per_library_root_maps_library_root_candidates() {
        let root = temp_root("redir-per-root");
        fs::create_dir_all(root.join("lib/etc/foo")).expect("library-root candidate");
        let dirs = [InferredDir {
            source: root.join("src"),
            prefix: root.join("src"),
            rel: PathBuf::from("etc/foo"),
            destination: PathBuf::from("lib/etc/foo"),
            kind: InferredKind::Data,
            size: 0,
            compiled: ["lib/etc/fooaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            objects: BTreeSet::new(),
        }];
        let refs: Vec<&InferredDir> = dirs.iter().collect();
        let rules = [descriptor(
            "proof",
            vec![group(
                "proof-g",
                RuntimeBase::LibraryRoot,
                "etc/foo",
                PathSelection::FirstPerLibraryRoot,
            )],
        )];
        let refs_rules: Vec<&Descriptor> = rules.iter().collect();
        let out = choose(&refs, &refs_rules, &["lib".to_string()], &root);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].mechanism, Mechanism::Env);
    }

    /// A file-kind group never selects a directory: the runtime's kind
    /// check rejects the directory copy, so the directory competes for
    /// an anchor instead.
    #[test]
    fn file_kind_group_never_selects_a_directory() {
        let root = temp_root("redir-file-kind");
        let dirs = [dir(
            &root,
            "etc/foo",
            &["etc/fooaaaaaaaaaaaaaaaaaaaaaaaaaaa"],
        )];
        let refs: Vec<&InferredDir> = dirs.iter().collect();
        let mut rules = descriptor("proof", Vec::new());
        rules.runtime.paths = vec![RuntimePathGroup {
            id: RuleId::new("proof-g").unwrap(),
            kind: ResourceKind::File,
            candidates: candidates(RuntimeBase::PackageRoot, &["etc/foo"]),
            variables: vec![EnvironmentName::new("PROOF_FILE").unwrap()],
            selection: PathSelection::First,
            inheritance: Inheritance::Replace,
        }];
        let refs_rules = [&rules];
        let out = choose(&refs, &refs_rules, &["lib".to_string()], &root);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].mechanism, Mechanism::Anchor);
    }

    /// A library-root candidate evaluated against a library directory
    /// beyond the bundle's own answers the launch before a later
    /// candidate can, the way the manifest's library list does, so the
    /// directory is not covered even though the bundle's own library
    /// directory would name it.
    #[test]
    fn earlier_candidate_on_another_library_root_blocks_coverage() {
        let root = temp_root("redir-multi-root");
        fs::create_dir_all(root.join("lib64/x")).expect("other root's match");
        let dirs = [dir(
            &root,
            "lib/fw",
            &["lib/fwaaaaaaaaaaaaaaaaaaaaaaaaaaaa"],
        )];
        let refs: Vec<&InferredDir> = dirs.iter().collect();
        let mut rules = descriptor("proof", Vec::new());
        rules.runtime.paths = vec![RuntimePathGroup {
            id: RuleId::new("proof-g").unwrap(),
            kind: ResourceKind::Directory,
            candidates: candidates(RuntimeBase::LibraryRoot, &["x", "fw"]),
            variables: vec![EnvironmentName::new("PROOF_DIR").unwrap()],
            selection: PathSelection::First,
            inheritance: Inheritance::Replace,
        }];
        let refs_rules = vec![&rules];
        let out = choose(
            &refs,
            &refs_rules,
            &["lib".to_string(), "lib64".to_string()],
            &root,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].mechanism, Mechanism::Anchor);
    }

    /// A selected list that cannot join into one value assigns nothing,
    /// the way the runtime drops the whole assignment on an
    /// unrepresentable value, so a path holding the separator covers
    /// nothing beside it.
    #[test]
    fn unrepresentable_selection_covers_nothing() {
        let root = temp_root("redir-join");
        fs::create_dir_all(root.join("share/ba:d")).expect("unrepresentable match");
        let dirs = [dir(
            &root,
            "share/fw",
            &["share/fwaaaaaaaaaaaaaaaaaaaaaaaaaa"],
        )];
        let refs: Vec<&InferredDir> = dirs.iter().collect();
        let mut rules = descriptor("proof", Vec::new());
        rules.runtime.paths = vec![RuntimePathGroup {
            id: RuleId::new("proof-g").unwrap(),
            kind: ResourceKind::Directory,
            candidates: candidates(RuntimeBase::PackageRoot, &["share/ba:d", "share/fw"]),
            variables: vec![EnvironmentName::new("PROOF_DIR").unwrap()],
            selection: PathSelection::All,
            inheritance: Inheritance::Replace,
        }];
        let refs_rules = vec![&rules];
        let out = choose(&refs, &refs_rules, &["lib".to_string()], &root);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].mechanism, Mechanism::Anchor);
    }

    /// A spelling exactly as long as the anchor path carries it; one
    /// byte short does not.
    #[test]
    fn spelling_length_boundary_at_the_anchor_path() {
        // "/dev/fd/77/a" is 12 bytes.
        let root = temp_root("redir-boundary");
        let dirs = [dir(&root, "lib/b", &["lib/b1234567"])];
        let refs: Vec<&InferredDir> = dirs.iter().collect();
        let out = choose(&refs, &[], &["lib".to_string()], &root);
        assert_eq!(out[0].mechanism, Mechanism::Anchor);

        let root = temp_root("redir-boundary-short");
        let dirs = [dir(&root, "lib/b", &["lib/b123456"])];
        let refs: Vec<&InferredDir> = dirs.iter().collect();
        let out = choose(&refs, &[], &["lib".to_string()], &root);
        assert_eq!(out[0].mechanism, Mechanism::Pair);
    }

    /// The final keys are never longer than the provisional ones the
    /// eligibility check used: with twenty-six short competitors ahead of
    /// it, the one eligible directory takes the key the filtered set
    /// gives it, the single letter, though its provisional key was the
    /// two-letter one.
    #[test]
    fn filtered_keys_shrink_with_the_set() {
        let root = temp_root("redir-shrink");
        let mut dirs: Vec<InferredDir> = (1..=26)
            .map(|n| {
                let dest = format!("lib/comp{n:02}");
                dir(&root, &dest, &[&dest])
            })
            .collect();
        dirs.push(dir(&root, "lib/zz", &["lib/zzaaaaaaaaaaaaaaaaaaaaaaaa"]));
        dirs.sort_by(|a, b| a.destination.cmp(&b.destination));
        let refs: Vec<&InferredDir> = dirs.iter().collect();
        let out = choose(&refs, &[], &["lib".to_string()], &root);
        assert_eq!(out.len(), 27);
        let eligible = out
            .iter()
            .find(|r| r.dest == Path::new("lib/zz"))
            .expect("the eligible directory is reported");
        assert_eq!(eligible.mechanism, Mechanism::Anchor);
        assert_eq!(eligible.key.as_deref(), Some("a"));
        assert!(
            out.iter()
                .all(|r| r.dest == Path::new("lib/zz") || r.mechanism == Mechanism::Pair),
            "every short competitor takes the overlay"
        );
    }

    const READ: u32 = 4;

    /// An anchored redirection over a directory with the given compiled
    /// spellings.
    fn anchored(dir: &InferredDir, key: &str) -> Redirection {
        Redirection {
            dest: dir.destination.clone(),
            mechanism: Mechanism::Anchor,
            key: Some(key.to_string()),
        }
    }

    fn anchor(key: &str, relative: &str) -> Redirection {
        Redirection {
            dest: PathBuf::from(relative),
            mechanism: Mechanism::Anchor,
            key: Some(key.to_string()),
        }
    }

    /// A sectionless 64-bit image with one read-only `PT_LOAD` mapping
    /// the whole file at a load address above its file offsets, and a
    /// `PT_DYNAMIC` segment pointing `DT_STRTAB` at a string table
    /// holding `dynstr` and `DT_STRSZ` at its length: the only way to
    /// the table after `sstrip`. The spellings in `rodata` follow the
    /// table in the same segment.
    fn biased_sectionless_image(dynstr: &str, rodata: &[&str]) -> Vec<u8> {
        let mut f = vec![0u8; 64];
        f[0..4].copy_from_slice(b"\x7fELF");
        f[4] = 2; // ELFCLASS64
        f[5] = 1; // ELFDATA2LSB
        f[6] = 1; // EV_CURRENT
        f[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
        f[18..20].copy_from_slice(&0x3eu16.to_le_bytes()); // EM_X86_64
        f[20..24].copy_from_slice(&1u32.to_le_bytes()); // e_version
        f[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
        f[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
        f[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
        f[56..58].copy_from_slice(&2u16.to_le_bytes()); // e_phnum
        f[58..60].copy_from_slice(&64u16.to_le_bytes()); // e_shentsize

        let load_ph = f.len();
        f.extend_from_slice(&[0u8; 56]);
        let dyn_ph = f.len();
        f.extend_from_slice(&[0u8; 56]);

        let mut strtab = vec![0u8];
        strtab.extend_from_slice(dynstr.as_bytes());
        strtab.push(0);
        let dynstr_off = f.len() as u64;
        f.extend_from_slice(&strtab);

        for spelling in rodata {
            f.extend_from_slice(spelling.as_bytes());
            f.push(0);
        }

        while !f.len().is_multiple_of(8) {
            f.push(0);
        }
        let dyn_off = f.len() as u64;
        for (tag, val) in [
            (goblin::elf::dynamic::DT_STRTAB, 0x400000u64 + dynstr_off),
            (goblin::elf::dynamic::DT_STRSZ, strtab.len() as u64),
            (0u64, 0u64),
        ] {
            f.extend_from_slice(&tag.to_le_bytes());
            f.extend_from_slice(&val.to_le_bytes());
        }
        let dyn_len = f.len() as u64 - dyn_off;

        let file_len = f.len() as u64;
        f[load_ph..load_ph + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
        f[load_ph + 4..load_ph + 8].copy_from_slice(&4u32.to_le_bytes()); // PF_R
        f[load_ph + 8..load_ph + 16].copy_from_slice(&0u64.to_le_bytes()); // p_offset
        f[load_ph + 16..load_ph + 24].copy_from_slice(&0x400000u64.to_le_bytes()); // p_vaddr
        f[load_ph + 32..load_ph + 40].copy_from_slice(&file_len.to_le_bytes()); // p_filesz
        f[load_ph + 40..load_ph + 48].copy_from_slice(&file_len.to_le_bytes()); // p_memsz
        f[load_ph + 48..load_ph + 56].copy_from_slice(&0x1000u64.to_le_bytes()); // p_align
        f[dyn_ph..dyn_ph + 4].copy_from_slice(&2u32.to_le_bytes()); // PT_DYNAMIC
        f[dyn_ph + 8..dyn_ph + 16].copy_from_slice(&dyn_off.to_le_bytes());
        f[dyn_ph + 16..dyn_ph + 24].copy_from_slice(&(0x400000u64 + dyn_off).to_le_bytes());
        f[dyn_ph + 32..dyn_ph + 40].copy_from_slice(&dyn_len.to_le_bytes());
        f[dyn_ph + 40..dyn_ph + 48].copy_from_slice(&dyn_len.to_le_bytes());
        f
    }

    /// Every exact occurrence of every spelling becomes the anchor path
    /// padded with NUL bytes to the original length, and nothing else
    /// changes: a longer path that shares the prefix without a separator
    /// keeps its bytes, a spelling preceded by other bytes is not a
    /// whole string, and the file length is unchanged.
    #[test]
    fn rewrite_hits_whole_spellings_only() {
        let root = temp_root("redir-rewrite");
        let dir = dir(&root, "share/fw", &["/usr/share/fw", "/usr/share/fwalias"]);
        let data = crate::bundle::infer::test_elf(&[(
            READ,
            &[
                "/usr/share/fw",
                "/usr/share/fwalias",
                "/usr/share/fw/sub",
                "x/usr/share/fw",
            ],
        )]);
        let file = root.join("lib/libfw.so");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, &data).unwrap();
        let redirection = anchored(&dir, "a");
        let (files, occurrences) =
            rewrite_anchors(&[redirection], &[&dir], std::slice::from_ref(&file)).unwrap();
        assert_eq!((files, occurrences), (1, 2));
        let rewritten = fs::read(&file).unwrap();
        assert_eq!(rewritten.len(), data.len(), "the file length is unchanged");
        assert!(rewritten.windows(14).any(|w| w == b"/dev/fd/77/a\0\0"));
        assert!(!rewritten.windows(13).any(|w| w == b"/usr/share/fw\0"));
        assert!(
            rewritten
                .windows("/usr/share/fw/sub\0".len())
                .any(|w| w == b"/usr/share/fw/sub\0"),
            "a longer path sharing the prefix keeps its bytes"
        );
        assert!(
            rewritten
                .windows("x/usr/share/fw\0".len())
                .any(|w| w == b"x/usr/share/fw\0"),
            "a spelling preceded by other bytes is not a whole string"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A file that holds none of the spellings is left alone and is not
    /// counted.
    #[test]
    fn rewrite_skips_files_without_spellings() {
        let root = temp_root("redir-rewrite-none");
        let dir = dir(&root, "share/fw", &["/usr/share/fw"]);
        let file = root.join("lib/other.so");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(
            &file,
            crate::bundle::infer::test_elf(&[(READ, &["/other/thing"])]),
        )
        .unwrap();
        let redirection = anchored(&dir, "a");
        let (files, occurrences) = rewrite_anchors(&[redirection], &[&dir], &[file]).unwrap();
        assert_eq!((files, occurrences), (0, 0));
        let _ = fs::remove_dir_all(&root);
    }

    /// The record names every anchored directory the tree still carries,
    /// the ledger holds it, and an empty or fully dropped set writes
    /// nothing.
    #[test]
    fn record_names_the_surviving_anchors() {
        let root = temp_root("redir-record");
        fs::create_dir_all(root.join("share/fw")).unwrap();
        fs::create_dir_all(root.join("lib/kept")).unwrap();
        let redirections = [
            anchor("a", "share/fw"),
            Redirection {
                dest: PathBuf::from("lib/kept"),
                mechanism: Mechanism::Env,
                key: None,
            },
            anchor("b", "share/gone"),
        ];
        assert_eq!(write_record(&redirections, &root).unwrap(), 1);
        let bytes = fs::read(root.join(".onelf/anchors")).unwrap();
        let decoded = onelf_format::anchors::decode(&bytes).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].key, "a");
        assert_eq!(decoded[0].relative, "share/fw");
        let ledger = fs::read_to_string(root.join(".onelf/generated")).unwrap();
        assert!(
            ledger.contains(".onelf/anchors"),
            "the ledger holds the record"
        );

        assert_eq!(write_record(&[], &root).unwrap(), 0);
        assert_eq!(
            write_record(&[anchor("a", "share/gone")], &root).unwrap(),
            0,
            "a dropped destination is not recorded"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A decision with no surviving anchors drops a record a previous
    /// build left, and its ledger entry with it: the packer refuses a
    /// record naming a directory the tree does not carry.
    #[test]
    fn record_removal_drops_a_stale_record_and_its_ledger_entry() {
        let root = temp_root("redir-record-stale");
        fs::create_dir_all(root.join("share/fw")).unwrap();
        assert_eq!(write_record(&[anchor("a", "share/fw")], &root).unwrap(), 1);
        assert!(root.join(".onelf/anchors").is_file());
        let ledger = fs::read_to_string(root.join(".onelf/generated")).unwrap();
        assert!(ledger.contains(".onelf/anchors"));

        fs::remove_dir_all(root.join("share/fw")).unwrap();
        assert_eq!(write_record(&[], &root).unwrap(), 0);
        assert!(!root.join(".onelf/anchors").exists());
        let ledger = fs::read_to_string(root.join(".onelf/generated")).unwrap();
        assert!(!ledger.contains(".onelf/anchors"));

        assert_eq!(write_record(&[], &root).unwrap(), 0);
        assert!(!root.join(".onelf/anchors").exists());
        let _ = fs::remove_dir_all(&root);
    }

    /// A mechanism resolves while the tree still holds its bundled
    /// directory, and not after the drop pass removed it.
    #[test]
    fn resolves_follows_the_tree() {
        let root = temp_root("redir-resolves");
        fs::create_dir_all(root.join("share/fw")).unwrap();
        assert!(anchor("a", "share/fw").resolves(&root));
        assert!(!anchor("a", "share/gone").resolves(&root));
        let _ = fs::remove_dir_all(&root);
    }

    fn persisted_anchor(key: &str, relative: &str) -> onelf_format::anchors::Anchor {
        onelf_format::anchors::Anchor {
            key: key.to_string(),
            relative: relative.to_string(),
        }
    }

    /// A quiet re-run, whose fresh decision is empty because the rewrite
    /// erased discovery's input, restores every persisted anchor the tree
    /// still carries, under the record's own keys.
    #[test]
    fn merge_restores_persisted_anchors_on_a_quiet_rerun() {
        let root = temp_root("redir-merge-quiet");
        fs::create_dir_all(root.join("share/fw")).unwrap();
        fs::create_dir_all(root.join("share/other")).unwrap();
        let persisted = [
            persisted_anchor("a", "share/fw"),
            persisted_anchor("b", "share/other"),
        ];
        let merged = merge(Vec::new(), &persisted, &root).unwrap();
        assert_eq!(
            merged,
            [anchor("a", "share/fw"), anchor("b", "share/other")]
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A persisted anchor whose destination the tree no longer carries
    /// leaves the decision, as long as no key behind it moves: the
    /// dropped entry sorted last, so the rest keeps its keys.
    #[test]
    fn merge_drops_a_persisted_anchor_whose_tree_left() {
        let root = temp_root("redir-merge-dropped");
        fs::create_dir_all(root.join("share/fw")).unwrap();
        let persisted = [
            persisted_anchor("a", "share/fw"),
            persisted_anchor("b", "share/gone"),
        ];
        let merged = merge(Vec::new(), &persisted, &root).unwrap();
        assert_eq!(merged, [anchor("a", "share/fw")]);
        let _ = fs::remove_dir_all(&root);
    }

    /// The record's keys are positional over its sorted entries, so a
    /// merge that would hand a carried destination a different key
    /// cannot stand in for the existing rewrites and stops the build.
    #[test]
    fn merge_refuses_to_move_a_persisted_key() {
        let root = temp_root("redir-merge-move");
        fs::create_dir_all(root.join("share/fw")).unwrap();
        let persisted = [
            persisted_anchor("a", "share/gone"),
            persisted_anchor("b", "share/fw"),
        ];
        let err = merge(Vec::new(), &persisted, &root).unwrap_err();
        assert!(
            err.to_string()
                .contains("anchor b names share/fw in the tree's record"),
            "{err}"
        );
        assert!(err.to_string().contains("bundle a fresh tree"), "{err}");
    }

    /// A fresh environment mapping for a destination the record anchors
    /// loses: the tree's bytes already name the anchor. A fresh mapping
    /// for any other destination passes through.
    #[test]
    fn merge_keeps_a_persisted_anchor_over_a_fresh_env() {
        let root = temp_root("redir-merge-env");
        fs::create_dir_all(root.join("share/fw")).unwrap();
        fs::create_dir_all(root.join("share/new")).unwrap();
        let fresh = vec![
            Redirection {
                dest: PathBuf::from("share/fw"),
                mechanism: Mechanism::Env,
                key: None,
            },
            Redirection {
                dest: PathBuf::from("share/new"),
                mechanism: Mechanism::Env,
                key: None,
            },
        ];
        let merged = merge(fresh, &[persisted_anchor("a", "share/fw")], &root).unwrap();
        assert_eq!(
            merged,
            [
                anchor("a", "share/fw"),
                Redirection {
                    dest: PathBuf::from("share/new"),
                    mechanism: Mechanism::Env,
                    key: None,
                },
            ]
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A spelling shorter than its anchor path cannot be rewritten in
    /// place, and the rewrite says so loudly instead of padding past the
    /// string's end. A spelling exactly one byte longer than the anchor
    /// path, whose window is the path plus its terminator, is the
    /// shortest one the rewrite accepts.
    #[test]
    fn rewrite_refuses_a_spelling_shorter_than_the_anchor_path() {
        let root = temp_root("redir-rewrite-short");
        let short_dir = dir(&root, "share/fw", &["/usr/fw"]);
        let redirection = anchored(&short_dir, "a");
        let err = rewrite_anchors(&[redirection], &[&short_dir], &[]).unwrap_err();
        assert!(
            err.to_string()
                .contains("/usr/fw is shorter than its anchor path /dev/fd/77/a"),
            "{err}"
        );
        let tight_dir = dir(&root, "share/aa", &["/usr/share/f"]);
        let err = rewrite_anchors(&[anchored(&tight_dir, "aa")], &[&tight_dir], &[]).unwrap_err();
        assert!(
            err.to_string()
                .contains("/usr/share/f is shorter than its anchor path /dev/fd/77/aa"),
            "{err}"
        );
        let fitting_dir = dir(&root, "share/ab", &["/usr/share/f"]);
        let (files, occurrences) =
            rewrite_anchors(&[anchored(&fitting_dir, "a")], &[&fitting_dir], &[]).unwrap();
        assert_eq!((files, occurrences), (0, 0), "no files to scan");
        let _ = fs::remove_dir_all(&root);
    }

    /// A sectionless object whose load address differs from its file
    /// offsets still keeps its dynamic string table out of the scan:
    /// goblin maps `DT_STRTAB` through the load bias, and the rewrite
    /// never touches the strings the loader reads.
    #[test]
    fn rewrite_excludes_dynstr_of_a_sectionless_biased_object() {
        let data = biased_sectionless_image("/usr/share/fw", &["/usr/share/fw"]);
        let elf = goblin::elf::Elf::parse(&data).unwrap();
        let (start, end) = crate::bundle::elf::dynstr_range(&elf).expect("sectionless dynstr");
        let raw = b"\0/usr/share/fw\0";
        assert_eq!(&data[start..end], raw);

        let root = temp_root("redir-rewrite-dynstr");
        let anchor_dir = dir(&root, "share/fw", &["/usr/share/fw"]);
        let file = root.join("lib/libfw.so");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, &data).unwrap();
        let (files, occurrences) = rewrite_anchors(
            &[anchored(&anchor_dir, "a")],
            &[&anchor_dir],
            std::slice::from_ref(&file),
        )
        .unwrap();
        assert_eq!(
            (files, occurrences),
            (1, 1),
            "only the rodata copy is rewritten"
        );
        let rewritten = fs::read(&file).unwrap();
        assert_eq!(
            &rewritten[start..end],
            raw,
            "the dynamic string table keeps its bytes"
        );
        assert!(
            rewritten[end..]
                .windows(14)
                .any(|w| w == b"/dev/fd/77/a\0\0"),
            "the rodata copy takes the anchor"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A fresh anchor's spellings are re-checked against the key the
    /// merged assignment handed out: the record's entries sort in
    /// ahead of the fresh ones, so a key `choose` found long enough
    /// can grow past a spelling. The record's own anchors are not
    /// re-checked: the merge keeps their keys, and their bytes carry
    /// exactly those keys.
    #[test]
    fn ensure_spellings_fit_refuses_a_fresh_anchor_the_merged_key_cannot_fit() {
        let root = temp_root("redir-fit");
        fs::create_dir_all(root.join("share/aaa")).unwrap();
        let fresh_dir = dir(&root, "share/fw", &["/usr/share/f"]);
        let persisted = [anchors::Anchor {
            key: "a".to_string(),
            relative: "share/aaa".to_string(),
        }];
        let err = ensure_spellings_fit(&[anchor("aa", "share/fw")], &persisted, &[&fresh_dir])
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("/usr/share/f is shorter than its anchor path /dev/fd/77/aa"),
            "{err}"
        );
        let recorded_dir = dir(&root, "share/aaa", &["/a"]);
        ensure_spellings_fit(
            &[anchor("aa", "share/aaa")],
            &persisted,
            &[&fresh_dir, &recorded_dir],
        )
        .expect("the record's own anchor is not re-checked");
        let _ = fs::remove_dir_all(&root);
    }

    /// A copy the build left owner-read-only is rewritten in place: the
    /// write grants the owner write only for the write itself, and the
    /// mode and the pinned mtime are what the copy step left.
    #[test]
    fn rewrite_restores_a_read_only_copy() {
        let root = temp_root("redir-rewrite-readonly");
        let ro_dir = dir(&root, "share/fw", &["/usr/share/fw"]);
        let data = crate::bundle::infer::test_elf(&[(READ, &["/usr/share/fw"])]);
        let file = root.join("lib/libfw.so");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, &data).unwrap();
        fs::set_permissions(&file, PermissionsExt::from_mode(0o500)).unwrap();
        let (files, occurrences) = rewrite_anchors(
            &[anchored(&ro_dir, "a")],
            &[&ro_dir],
            std::slice::from_ref(&file),
        )
        .unwrap();
        assert_eq!((files, occurrences), (1, 1));
        let meta = fs::metadata(&file).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o500, "mode restored");
        let mtime = meta.modified().unwrap();
        let expected = crate::pack::source_date_epoch()
            .map(|s| std::time::UNIX_EPOCH + std::time::Duration::from_secs(s))
            .unwrap_or(std::time::UNIX_EPOCH);
        assert_eq!(mtime, expected, "mtime re-pinned");
        let rewritten = fs::read(&file).unwrap();
        assert!(rewritten.windows(14).any(|w| w == b"/dev/fd/77/a\0\0"));
        let _ = fs::remove_dir_all(&root);
    }
}

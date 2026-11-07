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

use std::path::{Path, PathBuf};

use onelf_format::anchors;
use onelf_format::rules::{Descriptor, PathSelection, ResourceKind, RuntimeBase, RuntimePathGroup};

use super::infer::InferredDir;

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
}

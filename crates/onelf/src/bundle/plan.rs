//! Pre-mutation destination planning for the bundling walk.
//!
//! Before the walk writes anything, every destination it intends to
//! write is admitted into a plan: libraries and plugins under the
//! library directory, the data directories a carried library reads,
//! and the loader alias links. The plan resolves the destinations
//! against each other and against what the tree already holds, so a
//! build stops on the plan before the first copy when two resources
//! disagree, when a file and a directory claim one path, when a
//! destination would reach outside the tree, or when a destination
//! holds different bytes than the resource requires. The generated
//! ledger adjudicates destinations it knows: bytes it attested are the
//! build's own previous output, and anything else there is a
//! publisher's edit that stops publication. Nothing is written while a
//! finding stands.
//!
//! Sources are stamped at planning time, and the executor re-checks
//! each stamp before writing so a source that changed under the build
//! fails the assembly instead of mixing old and new bytes into the
//! tree.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use super::sysroot::{
    Disposition, GeneratedConflict, GeneratedEntry, GeneratedFinding, decide_generated_disposal,
    hash_entry, read_generated,
};

/// The largest tree walk admits, so a hostile or pathological source
/// tree answers with a bounded error instead of an unbounded walk.
const TREE_WALK_LIMIT: usize = 100_000;

/// The deepest tree walk descends, for the same bound.
const TREE_WALK_DEPTH: usize = 64;

/// The findings named before any other output, so the error stays
/// bounded no matter how many conflicts a tree produces.
const SHOWN_FINDINGS: usize = 8;

/// The origins a planned destination records before it starts counting
/// the rest.
const SHOWN_ORIGINS: usize = 4;

/// The kind of thing a destination is planned to hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlannedKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symlink.
    Link,
}

impl PlannedKind {
    /// The name the findings spell out.
    fn name(self) -> &'static str {
        match self {
            PlannedKind::File => "a file",
            PlannedKind::Dir => "a directory",
            PlannedKind::Link => "a link",
        }
    }
}

/// How a destination's existing content is judged, chosen per
/// destination class by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Existing {
    /// Anything already resolvable at the path satisfies it. The
    /// library directory's own copies are the build's previous output,
    /// and a re-bundle keeps them rather than rewriting them.
    SkipOnAny,
    /// The path is rewritten unconditionally; only the other planned
    /// resources can conflict with it.
    Overwrite,
}

/// What the executor does with one admitted destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    /// Write the planned content.
    Write,
    /// The destination is already satisfied; leave it as it stands.
    Skip,
    /// A finding was recorded; the destination is not written.
    Rejected,
}

/// Why planning stops a build before anything is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlanFinding {
    /// Two resources with differing verified content name one
    /// destination.
    ConflictingContent {
        /// The destination both resources write.
        dest: PathBuf,
        /// Where the first resource's bytes came from.
        first: String,
        /// Where the second resource's bytes came from.
        second: String,
        /// The first resource's content hash.
        first_hash: blake3::Hash,
        /// The second resource's content hash.
        second_hash: blake3::Hash,
    },
    /// One path is planned as one kind by a resource and as another
    /// kind by a second resource.
    KindClash {
        /// The destination both resources claim.
        dest: PathBuf,
        /// Where the first claim came from.
        first: String,
        /// Where the second claim came from.
        second: String,
    },
    /// A file-like destination sits inside another file destination,
    /// so one of the two can never be written.
    Overlap {
        /// The destination planned as a file.
        file: PathBuf,
        /// The destination planned inside it.
        under: PathBuf,
    },
    /// The destination exists with different content and nothing
    /// authorizes replacing it.
    PublisherCollision {
        /// The destination that already holds something else.
        dest: PathBuf,
        /// Where the planned bytes came from.
        source: String,
        /// What the path holds now.
        existing_kind: PlannedKind,
        /// The hash of what the path holds now.
        existing: blake3::Hash,
        /// The hash of what the plan would write.
        planned: blake3::Hash,
    },
    /// A source changed after planning named it, so the copy would
    /// mix bytes the plan never verified.
    SourceChanged {
        /// The source file that changed.
        source: PathBuf,
        /// The hash planning read.
        expected: blake3::Hash,
        /// The hash the source holds now.
        actual: blake3::Hash,
    },
    /// A planned destination would reach outside the tree.
    Escape {
        /// The destination that leaves the tree.
        dest: PathBuf,
        /// Where the destination came from.
        source: String,
    },
    /// A formerly generated path publication stops on, judged by the
    /// generated ledger's own disposition.
    Generated(GeneratedFinding),
}

impl PlanFinding {
    /// The path the finding is about, the sort and dedup key.
    fn path(&self) -> &Path {
        match self {
            PlanFinding::ConflictingContent { dest, .. }
            | PlanFinding::KindClash { dest, .. }
            | PlanFinding::PublisherCollision { dest, .. }
            | PlanFinding::Escape { dest, .. } => dest,
            PlanFinding::Overlap { file, .. } => file,
            PlanFinding::SourceChanged { source, .. } => source,
            PlanFinding::Generated(finding) => Path::new(&finding.path),
        }
    }

    /// A stable rank within one path, so equal findings dedup and the
    /// first class the plan found survives sorting.
    fn rank(&self) -> u8 {
        match self {
            PlanFinding::ConflictingContent { .. } => 0,
            PlanFinding::KindClash { .. } => 1,
            PlanFinding::Overlap { .. } => 2,
            PlanFinding::PublisherCollision { .. } => 3,
            PlanFinding::Escape { .. } => 4,
            PlanFinding::SourceChanged { .. } => 5,
            PlanFinding::Generated(_) => 6,
        }
    }
}

impl std::fmt::Display for PlanFinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanFinding::ConflictingContent {
                dest,
                first,
                second,
                first_hash,
                second_hash,
            } => write!(
                f,
                "{}: two resources write different bytes there: {} hashes to {}, {} hashes \
                 to {}; keep one and give the other a different destination",
                dest.display(),
                first,
                first_hash,
                second,
                second_hash
            ),
            PlanFinding::KindClash {
                dest,
                first,
                second,
            } => write!(
                f,
                "{}: {} plans a file there and {} plans a directory; give them different \
                 destinations",
                dest.display(),
                first,
                second
            ),
            PlanFinding::Overlap { file, under } => write!(
                f,
                "{}: planned as a file, but another resource also writes {} inside it; one \
                 of the two destinations has to go",
                file.display(),
                under.display()
            ),
            PlanFinding::PublisherCollision {
                dest,
                source,
                existing_kind,
                existing,
                planned,
            } => write!(
                f,
                "{}: holds {} content hashing to {}, but {} writes content hashing to {}; \
                 the build would replace it, so remove the existing path or move it to one \
                 the build does not write",
                dest.display(),
                existing_kind.name(),
                existing,
                source,
                planned
            ),
            PlanFinding::SourceChanged {
                source,
                expected,
                actual,
            } => write!(
                f,
                "{}: changed while the bundle was being written: planning read content \
                 hashing to {}, but it now hashes to {}; rerun the build",
                source.display(),
                expected,
                actual
            ),
            PlanFinding::Escape { dest, source } => write!(
                f,
                "{}: destination planned by {} reaches outside the tree",
                dest.display(),
                source
            ),
            PlanFinding::Generated(finding) => {
                let replaced = if finding.refreshable {
                    ", and this build would have replaced it"
                } else {
                    ", and no current output writes it anymore"
                };
                match &finding.why {
                    GeneratedConflict::Edited { recorded, actual } => write!(
                        f,
                        "{}: the ledger records blake3 {} but the file hashes to {}{}",
                        finding.path, recorded, actual, replaced
                    ),
                    GeneratedConflict::Unproven { actual } => write!(
                        f,
                        "{}: the ledger entry carries no hash, so the file's blake3 {} cannot \
                         be proven to be generated output{}",
                        finding.path, actual, replaced
                    ),
                }
            }
        }
    }
}

/// One planned destination: what the plan intends to write there, who
/// else names the path, and what the executor decided to do about what
/// the tree already holds.
#[derive(Debug, Clone)]
struct PlannedDest {
    kind: PlannedKind,
    hash: blake3::Hash,
    origins: Vec<String>,
    extra_origins: usize,
    decision: Admission,
}

/// The stamp planning took of one source file, re-checked before the
/// executor writes anything derived from it.
#[derive(Debug, Clone)]
struct SourceStamp {
    size: u64,
    modified: Option<std::time::SystemTime>,
    hash: blake3::Hash,
}

/// Every destination the walk intends to write, resolved against each
/// other and against the tree, with the findings that stop the build.
pub(crate) struct DestinationPlan {
    appdir: PathBuf,
    entries: BTreeMap<PathBuf, PlannedDest>,
    findings: Vec<PlanFinding>,
    sources: HashMap<PathBuf, SourceStamp>,
    ledger: BTreeMap<String, GeneratedEntry>,
}

/// The tagged hash of a regular file's bytes, the same construction
/// the generated ledger uses for a file.
pub(crate) fn file_hash(bytes: &[u8]) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"file ");
    hasher.update(bytes);
    hasher.finalize()
}

/// The tagged hash of a symlink's target text, the same construction
/// the generated ledger uses for a link.
pub(crate) fn link_hash(target: &str) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"link ");
    hasher.update(target.as_bytes());
    hasher.finalize()
}

/// The aggregate hash of a directory's admissible content: regular
/// files by bytes, relative symlinks by target text, names bound to
/// bytes in sorted order. Absolute symlinks and special files take no
/// part, since the data copier leaves them behind with a warning. An
/// `Err` answers for a tree past the walk's bounds, a walk that cannot
/// be planned.
pub(crate) fn tree_hash(root: &Path) -> io::Result<blake3::Hash> {
    let mut hasher = blake3::Hasher::new();
    let mut visited = 0usize;
    let mut stack = vec![(PathBuf::new(), root.to_path_buf(), 0usize)];
    while let Some((rel, abs, depth)) = stack.pop() {
        if depth > TREE_WALK_DEPTH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{}: deeper than the {TREE_WALK_DEPTH} level walk limit",
                    root.display()
                ),
            ));
        }
        let mut entries: Vec<fs::DirEntry> = match fs::read_dir(&abs) {
            Ok(entries) => entries.collect::<Result<Vec<_>, _>>()?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) if e.kind() == io::ErrorKind::NotADirectory => continue,
            Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", abs.display()))),
        };
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            visited += 1;
            if visited > TREE_WALK_LIMIT {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{}: more than {TREE_WALK_LIMIT} entries, past the walk limit",
                        root.display()
                    ),
                ));
            }
            let child_rel = rel.join(entry.file_name());
            let child = abs.join(entry.file_name());
            let kind = entry.file_type()?;
            if kind.is_dir() {
                stack.push((child_rel, child, depth + 1));
                continue;
            }
            hasher.update(child_rel.as_os_str().as_encoded_bytes());
            hasher.update(b"\0");
            if kind.is_file() {
                hasher.update(b"file ");
                hasher.update_reader(fs::File::open(&child)?)?;
            } else if kind.is_symlink() {
                let target = fs::read_link(&child)?;
                if target.is_relative() {
                    hasher.update(b"link ");
                    hasher.update(target.as_os_str().as_encoded_bytes());
                }
            }
        }
    }
    Ok(hasher.finalize())
}

/// Whether `rel` is a plain relative path with no way out of the tree.
fn is_plain_rel(rel: &Path) -> bool {
    !rel.as_os_str().is_empty() && rel.components().all(|c| matches!(c, Component::Normal(_)))
}

impl DestinationPlan {
    /// Start a plan for the AppDir at `appdir`, reading its generated
    /// ledger so planned destinations can be judged against what the
    /// build wrote there last.
    pub(crate) fn load(appdir: &Path) -> io::Result<Self> {
        let ledger = read_generated(appdir)?
            .into_iter()
            .map(|entry| (entry.path.clone(), entry))
            .collect();
        Ok(Self {
            appdir: appdir.to_path_buf(),
            entries: BTreeMap::new(),
            findings: Vec::new(),
            sources: HashMap::new(),
            ledger,
        })
    }

    /// Stamp a source file with what planning read, so the executor
    /// can tell when it changed before the copy runs.
    pub(crate) fn note_source(&mut self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let meta = fs::metadata(path)?;
        self.sources.insert(
            path.to_path_buf(),
            SourceStamp {
                size: meta.len(),
                modified: meta.modified().ok(),
                hash: file_hash(bytes),
            },
        );
        Ok(())
    }

    /// Verify a source still holds what planning read. A source whose
    /// content changed stops the assembly with a named finding; a
    /// source whose timestamps alone moved still matches by content.
    pub(crate) fn verify_source(&self, path: &Path) -> io::Result<()> {
        let Some(stamp) = self.sources.get(path) else {
            return Ok(());
        };
        let meta = match fs::metadata(path) {
            Ok(meta) => meta,
            Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
        };
        if meta.len() == stamp.size && meta.modified().ok() == stamp.modified {
            return Ok(());
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"file ");
        hasher.update_reader(fs::File::open(path)?)?;
        let actual = hasher.finalize();
        if actual == stamp.hash {
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            PlanFinding::SourceChanged {
                source: path.to_path_buf(),
                expected: stamp.hash,
                actual,
            }
            .to_string(),
        ))
    }

    /// Verify a directory source still holds the aggregate the plan
    /// recorded for the destination `rel`.
    pub(crate) fn verify_tree(&self, src: &Path, rel: &Path) -> io::Result<()> {
        let Some(entry) = self.entries.get(rel) else {
            return Ok(());
        };
        let actual = tree_hash(src)?;
        if actual == entry.hash {
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            PlanFinding::SourceChanged {
                source: src.to_path_buf(),
                expected: entry.hash,
                actual,
            }
            .to_string(),
        ))
    }

    /// Admit one planned file destination, `rel` relative to the
    /// AppDir, carrying `hash` of the verified content and named
    /// `origin` for the findings.
    pub(crate) fn admit_file(
        &mut self,
        rel: &Path,
        hash: blake3::Hash,
        origin: &str,
        existing: Existing,
    ) -> io::Result<Admission> {
        self.admit(rel, PlannedKind::File, hash, origin, existing)
    }

    /// Admit one planned symlink destination whose target text is
    /// `target`.
    pub(crate) fn admit_link(
        &mut self,
        rel: &Path,
        target: &str,
        origin: &str,
        existing: Existing,
    ) -> io::Result<Admission> {
        self.admit(rel, PlannedKind::Link, link_hash(target), origin, existing)
    }

    /// Admit one carried data resource, a file or a directory copied
    /// from `src`: a missing path is written, a tree-held directory is
    /// satisfied by presence because the build anchors its own
    /// configuration in place, and a tree-held file is judged by
    /// content, so a differing one is a publisher collision naming
    /// both roots.
    pub(crate) fn admit_carried(
        &mut self,
        rel: &Path,
        src: &Path,
        origin: &str,
    ) -> io::Result<Admission> {
        let meta = fs::symlink_metadata(src)?;
        let kind = if meta.is_dir() {
            PlannedKind::Dir
        } else {
            PlannedKind::File
        };
        let planned = if meta.is_dir() {
            tree_hash(src)?
        } else {
            file_hash(&fs::read(src)?)
        };
        if !is_plain_rel(rel) {
            self.findings.push(PlanFinding::Escape {
                dest: rel.to_path_buf(),
                source: origin.to_string(),
            });
            return Ok(Admission::Rejected);
        }
        if let Some(prev) = self.entries.get(rel).cloned() {
            if prev.hash == planned && prev.kind == kind {
                self.note_origin(rel, origin);
                return Ok(prev.decision);
            }
            self.record_clash(rel, &prev, origin, kind, planned);
            return Ok(Admission::Rejected);
        }
        let dest = self.appdir.join(rel);
        let decision = match fs::symlink_metadata(&dest) {
            Err(_) => Admission::Write,
            Ok(meta) if meta.is_dir() && kind == PlannedKind::Dir => {
                // A carried directory the tree already holds is the
                // tree's own: the build anchors its configuration in
                // place, so presence satisfies the carry.
                Admission::Skip
            }
            Ok(meta) => {
                let (existing_kind, actual) = if meta.is_dir() {
                    (PlannedKind::Dir, tree_hash(&dest)?)
                } else {
                    let existing_kind = if meta.file_type().is_symlink() {
                        PlannedKind::Link
                    } else {
                        PlannedKind::File
                    };
                    (existing_kind, hash_entry(&dest)?.unwrap_or(planned))
                };
                if actual == planned && existing_kind == kind {
                    Admission::Skip
                } else {
                    self.judge_carried(rel, existing_kind, actual, planned, origin)?
                }
            }
        };
        if decision != Admission::Rejected {
            self.entries.insert(
                rel.to_path_buf(),
                PlannedDest {
                    kind,
                    hash: planned,
                    origins: vec![origin.to_string()],
                    extra_origins: 0,
                    decision,
                },
            );
        }
        Ok(decision)
    }

    /// The findings the plan collected, sorted by path and class and
    /// deduplicated, so the same tree always reports the same list.
    pub(crate) fn findings(&self) -> Vec<PlanFinding> {
        let mut sorted = self.findings.clone();
        sorted.sort_by(|a, b| {
            (a.path().as_os_str(), a.rank()).cmp(&(b.path().as_os_str(), b.rank()))
        });
        sorted.dedup();
        sorted
    }

    /// Whether any finding stands.
    pub(crate) fn has_findings(&self) -> bool {
        !self.findings.is_empty()
    }

    /// The bounded error a failed plan stops the build with, before
    /// anything was written.
    pub(crate) fn error(&self) -> io::Error {
        let findings = self.findings();
        let mut text = format!(
            "destination planning stopped the build before anything was written; {} \
             conflict(s):\n",
            findings.len()
        );
        for finding in findings.iter().take(SHOWN_FINDINGS) {
            text.push_str("  ");
            text.push_str(&finding.to_string());
            text.push('\n');
        }
        if findings.len() > SHOWN_FINDINGS {
            text.push_str(&format!("  and {} more\n", findings.len() - SHOWN_FINDINGS));
        }
        text.push_str(
            "  resolve each one by removing or moving the named path, or by making the \
             resources agree\n",
        );
        io::Error::new(io::ErrorKind::InvalidData, text)
    }

    /// The shared admission path for file and link destinations.
    fn admit(
        &mut self,
        rel: &Path,
        kind: PlannedKind,
        hash: blake3::Hash,
        origin: &str,
        existing: Existing,
    ) -> io::Result<Admission> {
        if !is_plain_rel(rel) {
            self.findings.push(PlanFinding::Escape {
                dest: rel.to_path_buf(),
                source: origin.to_string(),
            });
            return Ok(Admission::Rejected);
        }
        if let Some(prev) = self.entries.get(rel).cloned() {
            if prev.kind == PlannedKind::Link && kind == PlannedKind::File {
                self.entries.insert(
                    rel.to_path_buf(),
                    PlannedDest {
                        kind,
                        hash,
                        origins: prev.origins,
                        extra_origins: prev.extra_origins,
                        decision: Admission::Write,
                    },
                );
                self.note_origin(rel, origin);
                return Ok(Admission::Write);
            }
            if prev.kind == PlannedKind::File && kind == PlannedKind::Link {
                return Ok(prev.decision);
            }
            if prev.hash == hash && prev.kind == kind {
                self.note_origin(rel, origin);
                return Ok(prev.decision);
            }
            self.record_clash(rel, &prev, origin, kind, hash);
            return Ok(Admission::Rejected);
        }
        if !self.check_nesting(rel, kind) {
            return Ok(Admission::Rejected);
        }
        let dest = self.appdir.join(rel);
        let decision = match existing {
            Existing::SkipOnAny => {
                if dest.exists() {
                    Admission::Skip
                } else {
                    Admission::Write
                }
            }
            Existing::Overwrite => Admission::Write,
        };
        if decision != Admission::Rejected {
            self.entries.insert(
                rel.to_path_buf(),
                PlannedDest {
                    kind,
                    hash,
                    origins: vec![origin.to_string()],
                    extra_origins: 0,
                    decision,
                },
            );
        }
        Ok(decision)
    }

    /// Decide what happens to existing content at a carried data
    /// destination: identical bytes satisfy the carry, the ledger's
    /// disposition answers for the build's own output, a legacy entry
    /// the plan rewrites is left for the copy to migrate, and anything
    /// else is a publisher collision.
    fn judge_carried(
        &mut self,
        rel: &Path,
        existing_kind: PlannedKind,
        actual: blake3::Hash,
        planned: blake3::Hash,
        origin: &str,
    ) -> io::Result<Admission> {
        if existing_kind == PlannedKind::File
            && let Some(entry) = self.ledger.get(rel.to_string_lossy().as_ref())
        {
            match decide_generated_disposal(entry, Some(actual)) {
                Disposition::Dispose | Disposition::Excuse => return Ok(Admission::Skip),
                Disposition::Absent => return Ok(Admission::Write),
                Disposition::Conflict(GeneratedConflict::Unproven { .. }) => {
                    return Ok(Admission::Skip);
                }
                Disposition::Conflict(why) => {
                    self.findings.push(PlanFinding::Generated(GeneratedFinding {
                        path: rel.to_string_lossy().into_owned(),
                        refreshable: true,
                        why,
                    }));
                    return Ok(Admission::Rejected);
                }
            }
        }
        self.findings.push(PlanFinding::PublisherCollision {
            dest: rel.to_path_buf(),
            source: origin.to_string(),
            existing_kind,
            existing: actual,
            planned,
        });
        Ok(Admission::Rejected)
    }

    /// Whether `rel` can hold a `kind` destination beside what the
    /// plan already holds: no file-like destination may sit inside
    /// another file destination, and a directory takes anything.
    fn check_nesting(&mut self, rel: &Path, kind: PlannedKind) -> bool {
        if kind == PlannedKind::Dir {
            return true;
        }
        for ancestor in rel.ancestors().skip(1) {
            if let Some(prev) = self.entries.get(ancestor)
                && prev.kind != PlannedKind::Dir
            {
                self.findings.push(PlanFinding::Overlap {
                    file: ancestor.to_path_buf(),
                    under: rel.to_path_buf(),
                });
                return false;
            }
        }
        let rel_buf = rel.to_path_buf();
        let under = self
            .entries
            .range((
                std::ops::Bound::Excluded(rel_buf.clone()),
                std::ops::Bound::Unbounded,
            ))
            .map(|(k, _)| k.clone())
            .take_while(|k| k.starts_with(&rel_buf))
            .next();
        if let Some(under) = under {
            self.findings.push(PlanFinding::Overlap {
                file: rel.to_path_buf(),
                under,
            });
            return false;
        }
        true
    }

    /// Record the finding for two resources disagreeing over one path.
    fn record_clash(
        &mut self,
        rel: &Path,
        prev: &PlannedDest,
        origin: &str,
        kind: PlannedKind,
        hash: blake3::Hash,
    ) {
        let first = prev
            .origins
            .first()
            .cloned()
            .unwrap_or_else(|| "a planned resource".to_string());
        if prev.kind != kind {
            self.findings.push(PlanFinding::KindClash {
                dest: rel.to_path_buf(),
                first,
                second: origin.to_string(),
            });
        } else {
            self.findings.push(PlanFinding::ConflictingContent {
                dest: rel.to_path_buf(),
                first,
                second: origin.to_string(),
                first_hash: prev.hash,
                second_hash: hash,
            });
        }
    }

    /// Note that another resource names a destination the plan already
    /// holds, so both origins are recorded.
    fn note_origin(&mut self, rel: &Path, origin: &str) {
        let Some(entry) = self.entries.get_mut(rel) else {
            return;
        };
        if entry.origins.len() < SHOWN_ORIGINS {
            entry.origins.push(origin.to_string());
        } else {
            entry.extra_origins += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::sysroot::record_generated;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("onelf-plan-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn seeded(tag: &str) -> (PathBuf, DestinationPlan) {
        let dir = tmp(tag);
        let plan = DestinationPlan::load(&dir).unwrap();
        (dir, plan)
    }

    /// Two resources with identical verified content targeting one
    /// destination deduplicate to one entry that records both origins.
    #[test]
    fn identical_destinations_deduplicate_to_one_entry() {
        let (_dir, mut plan) = seeded("dedupe");
        let hash = file_hash(b"same");
        let first = plan.admit_file(Path::new("lib/a.so"), hash, "one", Existing::SkipOnAny);
        let second = plan.admit_file(Path::new("lib/a.so"), hash, "two", Existing::SkipOnAny);
        assert_eq!(first.unwrap(), Admission::Write);
        assert_eq!(second.unwrap(), Admission::Write);
        assert!(!plan.has_findings());
        assert_eq!(plan.entries.len(), 1);
        let entry = plan.entries.values().next().unwrap();
        assert_eq!(entry.origins, vec!["one".to_string(), "two".to_string()]);
    }

    /// Two resources with differing content targeting one destination
    /// conflict, and the finding names both sources and both hashes.
    #[test]
    fn differing_destinations_conflict_naming_both_sources() {
        let (_dir, mut plan) = seeded("differ");
        let one = plan
            .admit_file(
                Path::new("lib/a.so"),
                file_hash(b"one"),
                "one",
                Existing::SkipOnAny,
            )
            .unwrap();
        let two = plan
            .admit_file(
                Path::new("lib/a.so"),
                file_hash(b"two"),
                "two",
                Existing::SkipOnAny,
            )
            .unwrap();
        assert_eq!(one, Admission::Write);
        assert_eq!(two, Admission::Rejected);
        let text = plan.error().to_string();
        assert!(text.contains("lib/a.so"), "{text}");
        assert!(text.contains("one"), "{text}");
        assert!(text.contains("two"), "{text}");
        assert!(
            text.contains(&file_hash(b"one").to_hex().to_string()),
            "{text}"
        );
        assert!(
            text.contains(&file_hash(b"two").to_hex().to_string()),
            "{text}"
        );
    }

    /// One path planned as a file by one resource and as a directory
    /// by another is a clash, and a file destination nested inside
    /// another file destination is an overlap.
    #[test]
    fn kind_clashes_and_nesting_overlap_are_findings() {
        let (dir, mut plan) = seeded("clash");
        fs::create_dir_all(dir.join("empty")).unwrap();
        let file = plan
            .admit_file(
                Path::new("lib/x"),
                file_hash(b"f"),
                "file rule",
                Existing::SkipOnAny,
            )
            .unwrap();
        let dir_rule = plan
            .admit_carried(Path::new("lib/x"), &dir.join("empty"), "dir rule")
            .unwrap();
        assert_eq!(file, Admission::Write);
        assert_eq!(dir_rule, Admission::Rejected);
        let text = plan.error().to_string();
        assert!(text.contains("plans a file"), "{text}");
        assert!(text.contains("dir rule"), "{text}");

        let (_dir, mut plan) = seeded("nest");
        plan.admit_file(
            Path::new("lib/x"),
            file_hash(b"f"),
            "outer",
            Existing::SkipOnAny,
        )
        .unwrap();
        let inner = plan
            .admit_file(
                Path::new("lib/x/y"),
                file_hash(b"g"),
                "inner",
                Existing::SkipOnAny,
            )
            .unwrap();
        assert_eq!(inner, Admission::Rejected);
        assert!(plan.error().to_string().contains("inside it"));
    }

    /// A destination that already exists with differing content is a
    /// publisher collision naming the existing path and the new
    /// source, and identical existing content is skipped as satisfied.
    #[test]
    fn existing_content_is_judged_by_hash() {
        let (dir, mut plan) = seeded("judge");
        fs::create_dir_all(dir.join("lib")).unwrap();
        fs::write(dir.join("lib/kept"), b"publisher bytes").unwrap();
        fs::write(dir.join("lib/same"), b"planned bytes").unwrap();
        fs::write(dir.join("src"), b"planned bytes").unwrap();
        let collision = plan
            .admit_carried(Path::new("lib/kept"), &dir.join("src"), "new source")
            .unwrap();
        assert_eq!(collision, Admission::Rejected);
        let satisfied = plan
            .admit_carried(Path::new("lib/same"), &dir.join("src"), "new source")
            .unwrap();
        assert_eq!(satisfied, Admission::Skip);
        let text = plan.error().to_string();
        assert!(text.contains("lib/kept"), "{text}");
        assert!(text.contains("new source"), "{text}");
        assert!(text.contains("would replace it"), "{text}");
    }

    /// A destination the ledger recorded as the build's own output is
    /// answered by its disposition: unchanged recorded bytes are kept
    /// as satisfied, and a publisher's edit stops the plan with the
    /// ledger's own finding.
    #[test]
    fn the_ledger_disposition_answers_for_generated_paths() {
        let (dir, _plan) = seeded("ledger");
        fs::create_dir_all(dir.join("lib/.onelf")).unwrap();
        fs::write(dir.join("lib/ours"), b"build output").unwrap();
        fs::write(dir.join("lib/edited"), b"build output").unwrap();
        record_generated(&dir, vec!["lib/ours".to_string(), "lib/edited".to_string()]).unwrap();
        fs::write(dir.join("lib/edited"), b"publisher edit").unwrap();
        fs::write(dir.join("src"), b"refreshed output").unwrap();
        let mut plan = DestinationPlan::load(&dir).unwrap();
        let ours = plan
            .admit_carried(Path::new("lib/ours"), &dir.join("src"), "walk")
            .unwrap();
        assert_eq!(ours, Admission::Skip, "the ledger's own output is kept");
        let edited = plan
            .admit_carried(Path::new("lib/edited"), &dir.join("src"), "walk")
            .unwrap();
        assert_eq!(edited, Admission::Rejected);
        let findings = plan.findings();
        assert!(
            matches!(findings.as_slice(), [PlanFinding::Generated(_)]),
            "{findings:?}"
        );
        assert!(plan.error().to_string().contains("ledger records"));
    }

    /// A source stamped at planning time verifies while its content
    /// stands and fails with a finding once its bytes change.
    #[test]
    fn a_changed_source_fails_verification_by_content() {
        let (dir, mut plan) = seeded("source");
        let src = dir.join("src.so");
        fs::write(&src, b"planned").unwrap();
        plan.note_source(&src, b"planned").unwrap();
        plan.verify_source(&src).unwrap();

        fs::write(&src, b"changed").unwrap();
        let err = plan.verify_source(&src).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("changed while the bundle"), "{text}");
        assert!(text.contains("src.so"), "{text}");
    }

    /// A destination that would reach outside the tree is an escape,
    /// whatever the resource.
    #[test]
    fn destinations_may_not_leave_the_tree() {
        let (_dir, mut plan) = seeded("escape");
        let out = plan
            .admit_file(
                Path::new("lib/../evil.so"),
                file_hash(b"x"),
                "hostile soname",
                Existing::SkipOnAny,
            )
            .unwrap();
        assert_eq!(out, Admission::Rejected);
        let text = plan.error().to_string();
        assert!(text.contains("outside the tree"), "{text}");
        assert!(text.contains("hostile soname"), "{text}");
    }

    /// Two directory resources for one destination deduplicate when
    /// their trees match, a tree-held directory is satisfied by
    /// presence because the build anchors its own configuration in
    /// place, and a changed source tree fails the executor's
    /// verification.
    #[test]
    fn directory_resources_judge_by_aggregate_content() {
        let (dir, mut plan) = seeded("tree");
        fs::create_dir_all(dir.join("src/one")).unwrap();
        fs::create_dir_all(dir.join("src/two")).unwrap();
        fs::write(dir.join("src/one/a.conf"), b"a").unwrap();
        fs::write(dir.join("src/two/a.conf"), b"a").unwrap();
        let first = plan
            .admit_carried(Path::new("share/data"), &dir.join("src/one"), "one")
            .unwrap();
        let second = plan
            .admit_carried(Path::new("share/data"), &dir.join("src/two"), "two")
            .unwrap();
        assert_eq!(first, Admission::Write);
        assert_eq!(second, Admission::Write);
        assert!(!plan.has_findings());

        fs::write(dir.join("src/two/a.conf"), b"different").unwrap();
        assert!(
            plan.verify_tree(&dir.join("src/two"), Path::new("share/data"))
                .is_err(),
            "a changed source tree fails verification"
        );

        let (dir, mut plan) = seeded("tree-existing");
        fs::create_dir_all(dir.join("share/data")).unwrap();
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("share/data/a.conf"),
            b"the tree's own patched copy",
        )
        .unwrap();
        fs::write(dir.join("src/a.conf"), b"different").unwrap();
        let judged = plan
            .admit_carried(Path::new("share/data"), &dir.join("src"), "carry")
            .unwrap();
        assert_eq!(judged, Admission::Skip);
        assert!(!plan.has_findings());
    }

    /// The aggregate tree hash binds file names to bytes, so two trees
    /// that differ only in layout hash differently and stay bounded on
    /// hostile input.
    #[test]
    fn the_tree_hash_binds_names_to_bytes_and_stays_bounded() {
        let (dir, _plan) = seeded("treehash");
        fs::create_dir_all(dir.join("a/sub")).unwrap();
        fs::write(dir.join("a/one"), b"one").unwrap();
        fs::write(dir.join("a/sub/two"), b"two").unwrap();
        std::os::unix::fs::symlink("one", dir.join("a/three")).unwrap();
        let one = tree_hash(&dir.join("a")).unwrap();

        fs::create_dir_all(dir.join("b/sub")).unwrap();
        fs::write(dir.join("b/one"), b"one").unwrap();
        fs::write(dir.join("b/sub/two"), b"two").unwrap();
        std::os::unix::fs::symlink("one", dir.join("b/three")).unwrap();
        let two = tree_hash(&dir.join("b")).unwrap();
        assert_eq!(one, two, "the same layout and bytes hash the same");

        fs::create_dir_all(dir.join("c/sub")).unwrap();
        fs::write(dir.join("c/two"), b"two").unwrap();
        fs::write(dir.join("c/sub/one"), b"one").unwrap();
        std::os::unix::fs::symlink("sub/one", dir.join("c/three")).unwrap();
        let moved = tree_hash(&dir.join("c")).unwrap();
        assert_ne!(one, moved, "a renamed file changes the hash");

        let mut deep = dir.join("deep");
        for _ in 0..=TREE_WALK_DEPTH {
            deep = deep.join("d");
        }
        fs::create_dir_all(deep).unwrap();
        assert!(tree_hash(&dir.join("deep")).is_err(), "depth is bounded");
    }
}

//! The build-time redirection choice for every carried inferred directory.
//!
//! Each directory a library compiled in reaches the application at launch
//! by exactly one mechanism, decided here when the package is built: a
//! descriptor's runtime mapping that already names the directory, else a
//! descriptor anchor its compiled-in strings can carry, else a path pair
//! for a namespace overlay. The choice is a pure function of the carried
//! directories, the descriptors, the assembled tree and the anchor record
//! a previous build left, so fixed inputs give fixed output.
//!
//! An anchor rewrites only a compiled-in path's first
//! [`anchors::ANCHOR_PREFIX_LEN`] bytes, so the key is the path's own tail
//! ([`anchors::key_of`]) and every string that starts with the directory,
//! the directory itself or a path below it, takes the same rewrite. A
//! directory carried inside another carried one takes its parent's
//! mechanism: the overlay binds the parent's tree path, the anchor's
//! rewrite reaches every string below the parent's spelling, and an
//! environment mapping covers a lookup that reaches the child through
//! the variable. A child spelling that names the parent through a
//! different alias, or names the child by its own absolute path while
//! only the variable redirects the parent, is not redirected by the
//! parent's choice. Only top-level carried directories are decided here.
//!
//! The record a previous build left (`.onelf/anchors`) folds into the
//! decision. Its keys are already in the tree's bytes, so they stay, and
//! a fresh spelling whose key would make one of them resolve somewhere
//! else takes a pair instead.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use onelf_format::anchors;
use onelf_format::paths::PathPair;

use super::infer::InferredDir;
use super::{Finding, Outcome, record_finding};
use onelf_format::rules::{Descriptor, PathSelection, ResourceKind, RuntimeBase, RuntimePathGroup};

/// How one carried inferred directory reaches the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mechanism {
    /// A descriptor's runtime mapping names the directory, so the launch
    /// sets its environment variable to the bundled copy and the
    /// compiled-in strings stay as they are.
    Env,
    /// Every compiled-in string starting with the directory is rewritten
    /// to start with the anchor root.
    Anchor,
    /// The compiled-in strings cannot carry an anchor: a path pair
    /// redirects a launch that has a namespace, and an unredirected
    /// launch is reported.
    Pair,
}

/// One carried inferred directory's choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Redirection {
    /// The canonical source directory the destination carries, for a
    /// choice this scan took: the identity a pair record matches on, so
    /// an omitted directory that shares the destination cannot stand in
    /// for the carried one. A redirection restored from the tree's own
    /// record names none, the record holding only the destination.
    pub source: Option<PathBuf>,
    /// The bundle directory, relative to the package root.
    pub dest: PathBuf,
    /// The mechanism chosen for it.
    pub mechanism: Mechanism,
    /// The anchor keys that resolve to it, for an anchor.
    pub keys: Vec<String>,
    /// The compiled-in spellings the rewrite reaches, for an anchor; for
    /// a pair, the spellings it answers, every one of the directory's
    /// when empty.
    pub spellings: Vec<String>,
}

/// The anchor keys taken so far: a key and the destination it resolves
/// to, plus the keys a previous record named for a destination the tree
/// no longer holds, which no fresh destination may take, since the bytes
/// that name them may still be in the tree.
struct Keys {
    taken: BTreeMap<String, PathBuf>,
    retired: HashSet<String>,
}

impl Keys {
    /// Whether `key` may resolve to `dest` alongside every key taken so
    /// far: no key is taken by another destination, and a key below
    /// another reaches, through that key's link, exactly the directory
    /// it stands for.
    fn fits(&self, key: &str, dest: &Path) -> bool {
        if self
            .retired
            .iter()
            .any(|old| old == key || anchors::key_below(key, old) || anchors::key_below(old, key))
        {
            return false;
        }
        self.taken.iter().all(|(taken, taken_dest)| {
            if taken == key {
                taken_dest == dest
            } else if anchors::key_below(key, taken) {
                dest == taken_dest.join(&key[taken.len() + 1..])
            } else if anchors::key_below(taken, key) {
                *taken_dest == dest.join(&taken[key.len() + 1..])
            } else {
                true
            }
        })
    }

    /// How many record lines the taken keys need: a key below another
    /// resolves through that one's link.
    fn lines(&self) -> usize {
        self.taken
            .keys()
            .filter(|key| !self.taken.keys().any(|k| anchors::key_below(key, k)))
            .count()
    }
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
/// `persisted` is the anchor record the tree already carries. A fresh
/// spelling a `recipe` pair names for another bundle directory takes no
/// key: the recipe's pair answers for it, so rewriting it would leave
/// that pair unused. A recorded destination keeps the keys its record
/// holds whatever a recipe says, since the tree's copies may already
/// carry them.
pub(crate) fn choose(
    dirs: &[&InferredDir],
    descriptors: &[&Descriptor],
    lib_dirs: &[String],
    directory: &Path,
    persisted: &[anchors::Anchor],
    recipe: &[PathPair],
) -> Vec<Redirection> {
    let claimed = |dir: &InferredDir, spelling: &str| {
        recipe
            .iter()
            .any(|pair| pair.absolute == spelling && Path::new(&pair.relative) != dir.destination)
    };
    let key = |dir: &InferredDir, spelling: &str| {
        if claimed(dir, spelling) {
            None
        } else {
            anchors::key_of(spelling)
        }
    };
    let mut keys = Keys {
        taken: BTreeMap::new(),
        retired: HashSet::new(),
    };
    let mut recorded: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    for anchor in persisted {
        if directory.join(&anchor.relative).is_dir() {
            keys.taken
                .insert(anchor.key.clone(), PathBuf::from(&anchor.relative));
            recorded
                .entry(PathBuf::from(&anchor.relative))
                .or_default()
                .push(anchor.key.clone());
        } else {
            keys.retired.insert(anchor.key.clone());
        }
    }
    let mut out: Vec<Redirection> = Vec::new();
    let mut rest: Vec<&InferredDir> = Vec::new();
    for dir in dirs {
        if recorded.contains_key(&dir.destination) {
            // The record anchors it already, so its bytes name the
            // anchor root and only an anchor reaches them; fresh
            // spellings join it where their keys fit.
            continue;
        }
        if env_covered(dir, descriptors, lib_dirs, directory) {
            out.push(Redirection {
                source: Some(dir.source.clone()),
                dest: dir.destination.clone(),
                mechanism: Mechanism::Env,
                keys: Vec::new(),
                spellings: Vec::new(),
            });
        } else {
            rest.push(dir);
        }
    }
    for (dest, mut taken) in recorded {
        let mut spellings = Vec::new();
        for dir in dirs.iter().filter(|dir| dir.destination == dest) {
            // A spelling that takes no key beside the recorded ones is
            // answered by a pair of its own.
            let mut leftover = Vec::new();
            for spelling in &dir.compiled {
                // A key the record holds stays, whatever a recipe says,
                // since the tree's copies may carry it; a fresh one
                // honours the recipe's claim.
                let recorded = anchors::key_of(spelling).filter(|key| taken.contains(key));
                match recorded.or_else(|| key(dir, spelling)) {
                    Some(key) if keys.fits(&key, &dest) => {
                        keys.taken.insert(key.clone(), dest.clone());
                        taken.push(key);
                        spellings.push(spelling.clone());
                    }
                    // The recipe's own pair answers a claimed spelling.
                    _ if claimed(dir, spelling) => {}
                    _ => leftover.push(spelling.clone()),
                }
            }
            if !leftover.is_empty() {
                out.push(Redirection {
                    source: Some(dir.source.clone()),
                    dest: dest.clone(),
                    mechanism: Mechanism::Pair,
                    keys: Vec::new(),
                    spellings: leftover,
                });
            }
        }
        taken.sort();
        taken.dedup();
        out.push(Redirection {
            source: None,
            dest,
            mechanism: Mechanism::Anchor,
            keys: taken,
            spellings,
        });
    }
    // The rest take anchors in sorted destination order, so a key two of
    // them would share goes to the same one every time. A destination
    // keeps its anchor only when every spelling it compiles in yields a
    // key that fits with the keys already taken; one that is not UTF-8
    // cannot enter the record, so it takes the overlay.
    rest.sort_by(|a, b| a.destination.cmp(&b.destination));
    for dir in rest {
        let wanted: Option<Vec<String>> = dir.destination.to_str().and_then(|_| {
            dir.compiled
                .iter()
                .map(|spelling| key(dir, spelling))
                .collect()
        });
        let fits = wanted.as_ref().is_some_and(|wanted| {
            let mut trial = Keys {
                taken: keys.taken.clone(),
                retired: keys.retired.clone(),
            };
            wanted.iter().all(|key| {
                let ok = trial.fits(key, &dir.destination);
                trial.taken.insert(key.clone(), dir.destination.clone());
                ok
            }) && trial.lines() <= anchors::MAX_ANCHORS
        });
        match wanted {
            Some(mut wanted) if fits => {
                for key in &wanted {
                    keys.taken.insert(key.clone(), dir.destination.clone());
                }
                wanted.sort();
                wanted.dedup();
                out.push(Redirection {
                    source: Some(dir.source.clone()),
                    dest: dir.destination.clone(),
                    mechanism: Mechanism::Anchor,
                    keys: wanted,
                    spellings: dir.compiled.iter().cloned().collect(),
                });
            }
            _ => out.push(Redirection {
                source: Some(dir.source.clone()),
                dest: dir.destination.clone(),
                mechanism: Mechanism::Pair,
                keys: Vec::new(),
                spellings: Vec::new(),
            }),
        }
    }
    sort_by_dest(out)
}

/// Report the choice per carried directory.
pub(crate) fn report(redirections: &[Redirection]) {
    if redirections.is_empty() {
        return;
    }
    eprintln!("  Redirected inferred directories:");
    for r in redirections {
        let detail = match r.mechanism {
            Mechanism::Env => "environment mapping".to_string(),
            Mechanism::Anchor => {
                let paths: Vec<String> = r
                    .keys
                    .iter()
                    .map(|key| format!("{}{key}", anchors::ANCHOR_PREFIX))
                    .collect();
                format!("anchor {}", paths.join(", "))
            }
            Mechanism::Pair => "path pair".to_string(),
        };
        eprintln!("    {}: {}", r.dest.display(), detail);
    }
}

impl Redirection {
    /// The mechanism as the completeness findings name it.
    pub(crate) fn describe(&self) -> String {
        match self.mechanism {
            Mechanism::Env => "redirected by its environment mapping".to_string(),
            Mechanism::Anchor => {
                let paths: Vec<String> = self
                    .keys
                    .iter()
                    .map(|key| format!("{}{key}", anchors::ANCHOR_PREFIX))
                    .collect();
                format!("redirected through {}", paths.join(", "))
            }
            Mechanism::Pair => {
                "redirected by a path pair where the launch has a mount namespace".to_string()
            }
        }
    }

    /// Whether the mechanism still names something: a carried directory
    /// the walk left out, every member of it condemned say, is not in
    /// the tree, and no mechanism reaches through an absent copy.
    pub(crate) fn resolves(&self, directory: &Path) -> bool {
        directory.join(&self.dest).is_dir()
    }
}

/// How each destination is redirected, for its finding: a destination
/// some of whose spellings take a pair beside its anchor is named with
/// both.
pub(crate) fn descriptions(redirections: &[Redirection]) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for r in redirections {
        out.entry(r.dest.display().to_string())
            .and_modify(|how| {
                how.push_str(", and ");
                how.push_str(&r.describe());
            })
            .or_insert_with(|| r.describe());
    }
    out
}

/// The pairs the surviving pair redirections add to the package's
/// `.onelf/paths` record: every compiled-in spelling of each pair
/// directory, with the bundle destination that answers for it, sorted
/// by absolute path. `dirs` is the set the choice was taken over, the
/// carried directories: an omitted directory that shares a carried
/// one's destination must not name its spellings into the record. A
/// destination that cannot enter the record, because it is not UTF-8,
/// contributes none and stays unredirected, which the completeness
/// findings report as an inference limit; so does a spelling the
/// codec refuses, or one past the record's pair bound (the record
/// holds `MAX_PAIRS`, of which the recipe's own pairs claim
/// `recipe.len()`), and the build goes on with the limit named. A
/// spelling one of the recipe's own pairs claims contributes none:
/// that pair answers for the path, so the spelling cannot spend a
/// bound slot a fresh one could use. Two directories compiling in one
/// spelling answer for it once: the first in the choice order wins,
/// and the rest stay unredirected unnamed.
pub(crate) fn record_pairs<'a>(
    survived: &[Redirection],
    dirs: impl IntoIterator<Item = &'a InferredDir>,
    recipe: &[PathPair],
) -> Vec<PathPair> {
    let dirs: Vec<&InferredDir> = dirs.into_iter().collect();
    let claimed: std::collections::HashSet<&str> =
        recipe.iter().map(|pair| pair.absolute.as_str()).collect();
    let bound = onelf_format::paths::MAX_PAIRS.saturating_sub(recipe.len());
    let mut out: BTreeMap<&str, PathPair> = BTreeMap::new();
    let mut dropped: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for choice in survived.iter().filter(|c| c.mechanism == Mechanism::Pair) {
        let Some(relative) = choice.dest.to_str() else {
            record_finding(Finding::new(
                choice.dest.display().to_string(),
                Outcome::Unchecked,
                "carried, but its pair cannot enter the paths record (the destination is not \
                 UTF-8), so its compiled-in paths stay unredirected",
            ));
            continue;
        };
        let Some(source) = &choice.source else {
            continue;
        };
        let Some(dir) = dirs
            .iter()
            .find(|dir| dir.destination == choice.dest && &dir.source == source)
        else {
            continue;
        };
        let spellings: Vec<&String> = if choice.spellings.is_empty() {
            dir.compiled.iter().collect()
        } else {
            choice.spellings.iter().collect()
        };
        for absolute in spellings {
            if claimed.contains(absolute.as_str()) {
                continue;
            }
            if out.contains_key(absolute.as_str()) {
                continue;
            }
            if let Some(why) = onelf_format::paths::pair_refusal(absolute, relative) {
                record_finding(Finding::new(
                    choice.dest.display().to_string(),
                    Outcome::Unchecked,
                    format!(
                        "carried, but its pair cannot enter the paths record ({why}), so its \
                         compiled-in paths stay unredirected"
                    ),
                ));
                continue;
            }
            if out.len() >= bound {
                dropped.insert(absolute.as_str());
                continue;
            }
            out.insert(
                absolute.as_str(),
                PathPair {
                    absolute: absolute.clone(),
                    relative: relative.to_string(),
                },
            );
        }
    }
    if !dropped.is_empty() {
        record_finding(Finding::new(
            "the paths record",
            Outcome::OptionalOmission,
            format!(
                "is full at its bound of {} pairs ({} reserved by the recipe's own, {} left for \
                 inference); {} compiled-in spelling(s) stay unredirected",
                onelf_format::paths::MAX_PAIRS,
                recipe.len(),
                bound,
                dropped.len()
            ),
        ));
    }
    out.into_values().collect()
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

/// Rewrite every compiled-in string that starts with an anchored
/// spelling, the directory itself or a path below it, in the bundled
/// copies: its first [`anchors::ANCHOR_PREFIX_LEN`] bytes become the
/// anchor root and the rest stays. Strings are whole NUL-terminated ones
/// in loadable read-only data, `.dynstr` excluded, and a spelling matches
/// only up to a separator, so a longer name that shares the prefix is
/// untouched. A string is judged by the longest of the anchored and the
/// `kept` spellings it is or lies below, so a path below an anchored
/// directory that a carried directory or a recipe pair answers for
/// differently ([`kept_spellings`]) keeps its bytes. A binary carrying
/// an embedded payload is left alone, as every other in-place writer
/// leaves it.
///
/// Returns the number of files changed and the number of strings
/// rewritten. The bytes land before the ledger attests them, so the
/// recorded hashes are the rewritten ones.
pub(crate) fn rewrite_anchors(
    redirections: &[Redirection],
    kept: &[&str],
    files: &[PathBuf],
) -> io::Result<(usize, usize)> {
    let mut spellings: HashMap<&[u8], bool> = kept
        .iter()
        .map(|spelling| (spelling.as_bytes(), false))
        .collect();
    let mut any = false;
    for spelling in redirections
        .iter()
        .filter(|r| r.mechanism == Mechanism::Anchor)
        .flat_map(|r| r.spellings.iter())
        .map(|spelling| spelling.as_bytes())
        .filter(|spelling| spelling.len() > anchors::ANCHOR_PREFIX_LEN)
    {
        spellings.insert(spelling, true);
        any = true;
    }
    if !any {
        return Ok((0, 0));
    }
    let mut files_changed = 0;
    let mut occurrences = 0;
    for file in files {
        let mut data = fs::read(file)?;
        if super::elf::has_embedded_payload(&data) {
            continue;
        }
        let ranges = super::infer::read_only_ranges(&data);
        let mut modified = false;
        for (start, end) in ranges {
            let mut at = start;
            while at < end {
                let len = data[at..end]
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(end - at);
                if anchored(&data[at..at + len], &spellings) {
                    data[at..at + anchors::ANCHOR_PREFIX_LEN]
                        .copy_from_slice(anchors::ANCHOR_PREFIX.as_bytes());
                    modified = true;
                    occurrences += 1;
                }
                at += len + 1;
            }
        }
        if modified {
            write_in_place(file, &data)?;
            files_changed += 1;
        }
    }
    Ok((files_changed, occurrences))
}

/// The spellings among `answers`, each with the bundle directory that
/// answers for it, that lie below an anchored spelling but name another
/// directory than the one the anchor's link would reach there, so the
/// enclosing rewrite must leave them alone. A spelling the anchor
/// reaches correctly, a directory nested in its anchored parent's copy
/// say, is left to that rewrite, which needs no namespace.
pub(crate) fn kept_spellings<'a>(
    redirections: &[Redirection],
    answers: &[(&'a str, &'a Path)],
) -> Vec<&'a str> {
    let anchored: Vec<(&str, &Path)> = redirections
        .iter()
        .filter(|r| r.mechanism == Mechanism::Anchor)
        .flat_map(|r| r.spellings.iter().map(|s| (s.as_str(), r.dest.as_path())))
        .collect();
    let mut out = Vec::new();
    for &(spelling, answer) in answers {
        if anchored.iter().any(|(a, _)| *a == spelling) {
            continue;
        }
        let enclosing = anchored
            .iter()
            .filter(|(a, _)| {
                spelling.len() > a.len()
                    && spelling.starts_with(a)
                    && spelling.as_bytes()[a.len()] == b'/'
            })
            .max_by_key(|(a, _)| a.len());
        if let Some((a, dest)) = enclosing
            && dest.join(&spelling[a.len() + 1..]) != answer
        {
            out.push(spelling);
        }
    }
    out
}

/// Where the anchor record `persisted` sends `spelling`: the directory
/// of the record's key for it, or of a recorded key it lies below.
pub(crate) fn recorded_reach(persisted: &[anchors::Anchor], spelling: &str) -> Option<PathBuf> {
    let key = anchors::key_of(spelling)?;
    persisted.iter().find_map(|anchor| {
        if anchor.key == key {
            Some(PathBuf::from(&anchor.relative))
        } else if anchors::key_below(&key, &anchor.key) {
            Some(Path::new(&anchor.relative).join(&key[anchor.key.len() + 1..]))
        } else {
            None
        }
    })
}

/// Whether any of `files` holds `spelling` as an earlier rewrite left
/// it: the anchor root over its first bytes, as a whole string or
/// followed by a separator.
pub(crate) fn rewritten_in(files: &[PathBuf], spelling: &str) -> io::Result<bool> {
    let Some(tail) = spelling.get(anchors::ANCHOR_PREFIX_LEN..) else {
        return Ok(false);
    };
    key_in_use(files, tail)
}

/// The anchored paths `files` carry: every string the anchor root
/// starts, with the separators after the root left out, the way a key
/// leaves them out.
pub(crate) fn anchor_tails(files: &[PathBuf]) -> io::Result<BTreeSet<String>> {
    let root = anchors::ANCHOR_PREFIX.as_bytes();
    let mut out = BTreeSet::new();
    for file in files {
        let data = fs::read(file)?;
        for (at, w) in data.windows(root.len()).enumerate() {
            if w != root {
                continue;
            }
            let rest = &data[at + root.len()..];
            let rest = &rest[rest.iter().take_while(|&&b| b == b'/').count()..];
            let end = rest
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(rest.len())
                .min(4096);
            if end > 0 {
                out.insert(String::from_utf8_lossy(&rest[..end]).into_owned());
            }
        }
    }
    Ok(out)
}

/// Whether any of `files` holds a string that resolves through the
/// anchor `key`: the anchor root, any further separators, since a key
/// leaves out the ones its path had there, and the key, whole or
/// followed by a separator.
pub(crate) fn key_in_use(files: &[PathBuf], key: &str) -> io::Result<bool> {
    let key = key.trim_start_matches('/').as_bytes();
    let root = anchors::ANCHOR_PREFIX.as_bytes();
    for file in files {
        let data = fs::read(file)?;
        let found = data.windows(root.len()).enumerate().any(|(at, w)| {
            if w != root {
                return false;
            }
            let rest = &data[at + root.len()..];
            let rest = &rest[rest.iter().take_while(|&&b| b == b'/').count()..];
            rest.starts_with(key) && matches!(rest.get(key.len()), Some(0 | b'/'))
        });
        if found {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether the longest of `spellings` that `string` is, or is a path
/// below, is an anchored one.
fn anchored(string: &[u8], spellings: &HashMap<&[u8], bool>) -> bool {
    if string.first() != Some(&b'/') || string.len() <= anchors::ANCHOR_PREFIX_LEN {
        return false;
    }
    if let Some(&anchor) = spellings.get(string) {
        return anchor;
    }
    (anchors::ANCHOR_PREFIX_LEN..string.len())
        .rev()
        .filter(|&i| string[i] == b'/')
        .find_map(|i| spellings.get(&string[..i]).copied())
        .unwrap_or(false)
}

/// Replace `file`'s bytes with `data` without ever leaving it half
/// written: a temporary sibling takes the bytes, the mode and the pinned
/// mtime, then replaces the file. The rename also leaves any hardlink to
/// the old inode alone.
fn write_in_place(file: &Path, data: &[u8]) -> io::Result<()> {
    let mode = fs::symlink_metadata(file)?.permissions().mode() & 0o7777;
    let name = file
        .file_name()
        .ok_or_else(|| io::Error::other(format!("{} names no file", file.display())))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".onelf-anchor-{}", std::process::id()));
    let tmp = file.with_file_name(tmp_name);
    let written = fs::write(&tmp, data)
        .and_then(|()| fs::set_permissions(&tmp, fs::Permissions::from_mode(mode)))
        .and_then(|()| {
            super::normalize_mtime(&tmp);
            fs::rename(&tmp, file)
        });
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

/// Write `.onelf/anchors` for the anchored redirections whose destination
/// the tree still carries, and record the file in the generated ledger.
/// A key below another resolves through that one's link, so only the
/// outermost keys enter the record. Returns the number of records
/// written. An empty decision drops a record a previous build left, with
/// its ledger entry.
pub(crate) fn write_record(redirections: &[Redirection], directory: &Path) -> io::Result<usize> {
    let mut taken: BTreeMap<&str, &Path> = BTreeMap::new();
    for r in redirections
        .iter()
        .filter(|r| r.mechanism == Mechanism::Anchor && r.resolves(directory))
    {
        for key in &r.keys {
            taken.insert(key, &r.dest);
        }
    }
    let anchored: Vec<anchors::Anchor> = taken
        .iter()
        .filter(|(key, _)| !taken.keys().any(|k| anchors::key_below(key, k)))
        .map(|(key, dest)| anchors::Anchor {
            key: (*key).to_owned(),
            relative: dest.to_string_lossy().into_owned(),
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

    /// A pair redirection records every compiled-in spelling of its
    /// directory, sorted by absolute path, and an anchored directory
    /// records none: its spellings are rewritten, not overlaid.
    #[test]
    fn pair_choices_record_every_spelling_sorted() {
        let root = temp_root("redir-pairs-record");
        let dirs = [
            dir(&root, "lib/short", &["/usr/share/s", "/usr/lib/s2"]),
            dir(&root, "lib/loong", &["/usr/share/looooooooong"]),
        ];
        let refs: Vec<&InferredDir> = dirs.iter().collect();
        let out = choose(&refs, &[], &["lib".to_string()], &root, &[], &[]);
        assert_eq!(out.len(), 2);
        let pairs = record_pairs(&out, &dirs, &[]);
        assert_eq!(
            pairs,
            vec![
                PathPair {
                    absolute: "/usr/lib/s2".to_string(),
                    relative: "lib/short".to_string(),
                },
                PathPair {
                    absolute: "/usr/share/s".to_string(),
                    relative: "lib/short".to_string(),
                },
            ]
        );
    }

    /// A destination that cannot enter the record, because it is not
    /// UTF-8, contributes no pair and stays unredirected; the choice
    /// gives it to the overlay and the findings report the limit.
    #[test]
    fn pair_choices_skip_a_destination_outside_the_record() {
        use std::os::unix::ffi::OsStrExt;
        let root = temp_root("redir-pairs-non-utf8");
        let mut raw = b"lib/fw".to_vec();
        raw.push(0x80);
        let bad = InferredDir {
            destination: PathBuf::from(std::ffi::OsStr::from_bytes(&raw)),
            compiled: ["/usr/share/fw".to_string()].into(),
            ..dir(&root, "lib/kept", &["/usr/share/kept"])
        };
        let dirs = [bad];
        let survived = [Redirection {
            source: Some(dirs[0].source.clone()),
            dest: PathBuf::from(std::ffi::OsStr::from_bytes(&raw)),
            mechanism: Mechanism::Pair,
            keys: Vec::new(),
            spellings: Vec::new(),
        }];
        assert!(record_pairs(&survived, &dirs, &[]).is_empty());
        // The limit is named durably: the completeness findings carry
        // the destination and why its pair stayed out of the record.
        super::super::completeness::report(&[]);
        let findings = crate::bundle::take_findings();
        assert!(
            findings.iter().any(|f| f.object.contains("lib/fw")
                && f.reason.contains("cannot enter the paths record")),
            "{findings:?}"
        );
    }

    /// Spellings past the record's pair bound stay out, and the
    /// finding names the limit: the build goes on with the rest of the
    /// directory's paths redirected.
    #[test]
    fn pair_choices_past_the_record_bound_are_named_and_dropped() {
        let root = temp_root("redir-pairs-bound");
        let spellings: Vec<String> = (0..onelf_format::paths::MAX_PAIRS + 6)
            .map(|i| format!("/u/s{i}"))
            .collect();
        let refs: Vec<&str> = spellings.iter().map(String::as_str).collect();
        let dirs = [dir(&root, "lib/fw", &refs)];
        let refs: Vec<&InferredDir> = dirs.iter().collect();
        let out = choose(&refs, &[], &["lib".to_string()], &root, &[], &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].mechanism, Mechanism::Pair);
        let pairs = record_pairs(&out, &dirs, &[]);
        assert_eq!(pairs.len(), onelf_format::paths::MAX_PAIRS);
        super::super::completeness::report(&[]);
        let findings = crate::bundle::take_findings();
        assert!(
            findings.iter().any(|f| {
                f.object == "the paths record"
                    && f.reason
                        .contains("6 compiled-in spelling(s) stay unredirected")
            }),
            "{findings:?}"
        );
    }

    /// The recipe's own pairs reserve room in the record: inference's
    /// cap is the bound less the recipe's count, and the finding names
    /// both numbers.
    #[test]
    fn the_recipe_pairs_reserve_the_record_room() {
        let root = temp_root("redir-pairs-reserved");
        let spellings: Vec<String> = (0..8).map(|i| format!("/u/s{i}")).collect();
        let refs: Vec<&str> = spellings.iter().map(String::as_str).collect();
        let dirs = [dir(&root, "lib/fw", &refs)];
        let refs: Vec<&InferredDir> = dirs.iter().collect();
        let out = choose(&refs, &[], &["lib".to_string()], &root, &[], &[]);
        let recipe: Vec<PathPair> = (0..onelf_format::paths::MAX_PAIRS - 5)
            .map(|i| PathPair {
                absolute: format!("/u/r{i}"),
                relative: "lib/recipe".to_string(),
            })
            .collect();
        let pairs = record_pairs(&out, &dirs, &recipe);
        assert_eq!(pairs.len(), 5);
        super::super::completeness::report(&[]);
        let findings = crate::bundle::take_findings();
        assert!(
            findings.iter().any(|f| {
                f.reason
                    .contains("3 compiled-in spelling(s) stay unredirected")
                    && f.reason.contains("5 left for inference")
            }),
            "{findings:?}"
        );
    }

    /// A spelling the recipe's own pair claims spends no bound slot:
    /// the fresh spelling beside it takes the room the claimed one
    /// would have burned, and no dropped finding names either.
    #[test]
    fn a_recipe_claimed_spelling_spends_no_record_slot() {
        let mut recipe: Vec<PathPair> = (0..onelf_format::paths::MAX_PAIRS - 2)
            .map(|i| PathPair {
                absolute: format!("/u/r{i}"),
                relative: "lib/recipe".to_string(),
            })
            .collect();
        recipe.push(PathPair {
            absolute: "/u/claimed".to_string(),
            relative: "lib/recipe".to_string(),
        });
        let root = temp_root("redir-pairs-claimed");
        let refs = ["/u/claimed", "/u/fresh"];
        let dirs = [dir(&root, "lib/fw", &refs)];
        let refs: Vec<&InferredDir> = dirs.iter().collect();
        let out = choose(&refs, &[], &["lib".to_string()], &root, &[], &[]);
        let pairs = record_pairs(&out, &dirs, &recipe);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].absolute, "/u/fresh");
        super::super::completeness::report(&[]);
        let findings = crate::bundle::take_findings();
        assert!(
            findings
                .iter()
                .all(|f| !f.reason.contains("stay unredirected")),
            "{findings:?}"
        );
    }

    /// A pair redirection matches its directory by source as well as
    /// destination: an omitted directory that shares the carried one's
    /// destination cannot name its spellings into the record, even when
    /// the caller's set still holds it.
    #[test]
    fn pair_choices_match_the_carried_directory_by_source() {
        let root = temp_root("redir-pairs-decoy");
        let carried = dir(&root, "lib/fw", &["/usr/lib/s"]);
        let decoy = InferredDir {
            source: root.join("other/src"),
            ..dir(&root, "lib/fw", &["/decoy/spelling"])
        };
        let dirs = [decoy, carried];
        let refs: Vec<&InferredDir> = dirs[1..].iter().collect();
        let out = choose(&refs, &[], &["lib".to_string()], &root, &[], &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].mechanism, Mechanism::Pair);
        let pairs = record_pairs(&out, &dirs, &[]);
        assert_eq!(
            pairs,
            vec![PathPair {
                absolute: "/usr/lib/s".to_string(),
                relative: "lib/fw".to_string(),
            }]
        );
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
        let out = choose(&refs, &refs_rules, &["lib".to_string()], &root, &[], &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].mechanism, Mechanism::Env);
        assert!(out[0].keys.is_empty());
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
        let out = choose(&refs, &[], &["lib".to_string()], &root, &[], &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].mechanism, Mechanism::Anchor);
        assert_eq!(out[0].keys, vec!["a".repeat(19)]);
        assert_eq!(
            out[0].spellings,
            vec!["lib/appdiraaaaaaaaaaaaaaaaaaaa".to_string()]
        );
    }

    /// A directory whose only compiled spelling is too short for the
    /// anchor path takes the overlay path pair.
    #[test]
    fn pair_when_strings_are_short() {
        let root = temp_root("redir-pair");
        let dirs = [dir(&root, "lib/s", &["lib/s"])];
        let refs: Vec<&InferredDir> = dirs.iter().collect();
        let out = choose(&refs, &[], &["lib".to_string()], &root, &[], &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].mechanism, Mechanism::Pair);
        assert!(out[0].keys.is_empty());
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
        let out = choose(&refs, &refs_rules, &["lib".to_string()], &root, &[], &[]);
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
            &[],
            &[],
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
        let out = choose(&refs, &refs_rules, &["lib".to_string()], &root, &[], &[]);
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
        let out = choose(&refs, &refs_rules, &["lib".to_string()], &root, &[], &[]);
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
        let out = choose(&refs, &refs_rules, &["lib".to_string()], &root, &[], &[]);
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
        let out = choose(&refs, &refs_rules, &["lib".to_string()], &root, &[], &[]);
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
            &[],
            &[],
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
        let out = choose(&refs, &refs_rules, &["lib".to_string()], &root, &[], &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].mechanism, Mechanism::Anchor);
    }

    /// A spelling one byte longer than the anchor prefix carries a key;
    /// one no longer than the prefix leaves nothing to key on.
    #[test]
    fn spelling_length_boundary_at_the_anchor_path() {
        // "/dev/fd/77/" is 11 bytes.
        let root = temp_root("redir-boundary");
        let dirs = [dir(&root, "lib/b", &["lib/b1234567"])];
        let refs: Vec<&InferredDir> = dirs.iter().collect();
        let out = choose(&refs, &[], &["lib".to_string()], &root, &[], &[]);
        assert_eq!(out[0].mechanism, Mechanism::Anchor);

        let root = temp_root("redir-boundary-short");
        let dirs = [dir(&root, "lib/b", &["lib/b123456"])];
        let refs: Vec<&InferredDir> = dirs.iter().collect();
        let out = choose(&refs, &[], &["lib".to_string()], &root, &[], &[]);
        assert_eq!(out[0].mechanism, Mechanism::Pair);
    }

    const READ: u32 = 4;

    /// An anchored redirection over a directory with the given compiled
    /// spellings, keyed by their tails.
    fn anchored(dir: &InferredDir) -> Redirection {
        Redirection {
            source: Some(dir.source.clone()),
            dest: dir.destination.clone(),
            mechanism: Mechanism::Anchor,
            keys: dir
                .compiled
                .iter()
                .filter_map(|s| anchors::key_of(s))
                .collect(),
            spellings: dir.compiled.iter().cloned().collect(),
        }
    }

    fn anchor(key: &str, relative: &str) -> Redirection {
        Redirection {
            source: None,
            dest: PathBuf::from(relative),
            mechanism: Mechanism::Anchor,
            keys: vec![key.to_string()],
            spellings: Vec::new(),
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

    /// Every string that starts with an anchored spelling, the directory
    /// itself or a path below it, takes the anchor root over its first
    /// bytes and keeps the rest; nothing else changes. A longer name that
    /// shares the prefix without a separator keeps its bytes, a spelling
    /// preceded by other bytes is not a whole string, and every string
    /// keeps its length, so a library measuring it sees no padding.
    #[test]
    fn rewrite_hits_whole_spellings_only() {
        let root = temp_root("redir-rewrite");
        let dir = dir(&root, "share/fw", &["/usr/share/fw", "/usr/share/fwalias"]);
        let data = crate::bundle::infer::test_elf(&[(
            READ,
            &[
                "/usr/share/fw",
                "/usr/share/fwalias",
                "/usr/share/fw/sub/a.conf",
                "/usr/share/fwx",
                "x/usr/share/fw",
            ],
        )]);
        let file = root.join("lib/libfw.so");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, &data).unwrap();
        let (files, occurrences) =
            rewrite_anchors(&[anchored(&dir)], &[], std::slice::from_ref(&file)).unwrap();
        assert_eq!((files, occurrences), (1, 3));
        let rewritten = fs::read(&file).unwrap();
        assert_eq!(rewritten.len(), data.len(), "the file length is unchanged");
        let has = |needle: &[u8]| rewritten.windows(needle.len()).any(|w| w == needle);
        assert!(has(b"\0/dev/fd/77/fw\0"));
        assert!(has(b"\0/dev/fd/77/fwalias\0"));
        assert!(
            has(b"\0/dev/fd/77/fw/sub/a.conf\0"),
            "a path below the directory"
        );
        assert!(!has(b"\0/usr/share/fw\0"));
        assert!(has(b"\0/usr/share/fwx\0"), "a longer name keeps its bytes");
        assert!(
            has(b"x/usr/share/fw\0"),
            "a spelling preceded by other bytes is not a whole string"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A shorter string the linker stored inside a rewritten path's tail
    /// keeps its bytes: only the prefix moves, so `"input"` inside
    /// `"/usr/share/libinput"` still reads `"input"`.
    #[test]
    fn a_tail_merged_string_survives_the_rewrite() {
        let root = temp_root("redir-rewrite-tail");
        let dir = dir(&root, "share/libinput", &["/usr/share/libinput"]);
        let data = crate::bundle::infer::test_elf(&[(READ, &["/usr/share/libinput"])]);
        let at = data
            .windows(b"/usr/share/libinput\0".len())
            .position(|w| w == b"/usr/share/libinput\0")
            .unwrap();
        let file = root.join("lib/libinput.so");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, &data).unwrap();
        rewrite_anchors(&[anchored(&dir)], &[], std::slice::from_ref(&file)).unwrap();
        let rewritten = fs::read(&file).unwrap();
        assert_eq!(&rewritten[at..at + 20], b"/dev/fd/77/libinput\0");
        assert_eq!(&rewritten[at + 14..at + 20], b"input\0", "the inner string");
        assert_eq!(
            &rewritten[at + 11..at + 20],
            b"libinput\0",
            "the inner string"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A path below an anchored directory that another spelling answers
    /// for keeps its bytes: the longest spelling a string starts with
    /// decides, so a pair below an anchor is not overridden by it.
    #[test]
    fn rewrite_leaves_a_path_another_spelling_answers() {
        let root = temp_root("redir-rewrite-longest");
        let parent = dir(&root, "share/alsa", &["/usr/share/alsa"]);
        let file = root.join("lib/libfw.so");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(
            &file,
            crate::bundle::infer::test_elf(&[(
                READ,
                &[
                    "/usr/share/alsa/alsa.conf",
                    "/usr/share/alsa/other",
                    "/usr/share/alsa/other/x.conf",
                ],
            )]),
        )
        .unwrap();
        let (_, occurrences) = rewrite_anchors(
            &[anchored(&parent)],
            &["/usr/share/alsa/other"],
            std::slice::from_ref(&file),
        )
        .unwrap();
        assert_eq!(occurrences, 1);
        let rewritten = fs::read(&file).unwrap();
        let has = |needle: &[u8]| rewritten.windows(needle.len()).any(|w| w == needle);
        assert!(has(b"/dev/fd/77/alsa/alsa.conf\0"));
        assert!(has(b"/usr/share/alsa/other\0"));
        assert!(has(b"/usr/share/alsa/other/x.conf\0"));
        let _ = fs::remove_dir_all(&root);
    }

    /// A spelling below an anchored one is kept out of the rewrite only
    /// when it names another directory than the anchor's link reaches
    /// there; one the anchor reaches correctly is left to it.
    #[test]
    fn only_a_differently_answered_nested_spelling_is_kept() {
        let root = temp_root("redir-kept");
        let parent = dir(&root, "share/alsa", &["/usr/share/alsa"]);
        let nested = dir(&root, "share/alsa/ucm2", &["/usr/share/alsa/ucm2"]);
        let stray = dir(&root, "share/elsewhere", &["/usr/share/alsa/other"]);
        let outside = dir(&root, "share/fw", &["/usr/share/fw"]);
        let kept = kept_spellings(
            &[anchored(&parent)],
            &[&parent, &nested, &stray, &outside]
                .iter()
                .flat_map(|d| {
                    d.compiled
                        .iter()
                        .map(|s| (s.as_str(), d.destination.as_path()))
                })
                .collect::<Vec<_>>(),
        );
        assert_eq!(kept, vec!["/usr/share/alsa/other"]);
    }

    /// The anchored paths a tree object carries are read with the
    /// separators after the anchor root left out.
    #[test]
    fn anchor_tails_reads_every_anchored_path() {
        let root = temp_root("redir-tails");
        let file = root.join("lib/libfw.so");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(
            &file,
            crate::bundle::infer::test_elf(&[(
                READ,
                &["/dev/fd/77/fw/a.conf", "/dev/fd/77//share/wfw"],
            )]),
        )
        .unwrap();
        let tails = anchor_tails(std::slice::from_ref(&file)).unwrap();
        assert_eq!(
            tails.into_iter().collect::<Vec<_>>(),
            vec!["fw/a.conf".to_string(), "share/wfw".to_string()]
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A spelling a recipe pair names for another directory takes no
    /// key, so the directory takes the overlay and the recipe's pair
    /// answers for it; a recipe pair that agrees leaves the anchor.
    #[test]
    fn a_recipe_pair_for_another_directory_takes_the_spelling() {
        let root = temp_root("redir-claimed");
        let fw = dir(&root, "share/fw", &["/usr/share/fw"]);
        let refs: Vec<&InferredDir> = vec![&fw];
        let pair = |relative: &str| PathPair {
            absolute: "/usr/share/fw".to_string(),
            relative: relative.to_string(),
        };
        let lib = ["lib".to_string()];
        let out = choose(&refs, &[], &lib, &root, &[], &[pair("share/other")]);
        assert_eq!(out[0].mechanism, Mechanism::Pair);
        let out = choose(&refs, &[], &lib, &root, &[], &[pair("share/fw")]);
        assert_eq!(out[0].mechanism, Mechanism::Anchor);
        assert_eq!(
            kept_spellings(&out, &[("/usr/share/fw/x", Path::new("share/other"))]),
            vec!["/usr/share/fw/x"]
        );
        assert!(kept_spellings(&out, &[("/usr/share/fw/x", Path::new("share/fw/x"))]).is_empty());
        // A destination the record already anchors keeps its key, since
        // the tree's copies may carry it.
        fs::create_dir_all(root.join("share/fw")).unwrap();
        let persisted = [anchors::Anchor {
            key: "fw".to_string(),
            relative: "share/fw".to_string(),
        }];
        let out = choose(&refs, &[], &lib, &root, &persisted, &[pair("share/other")]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].mechanism, Mechanism::Anchor);
        // A spelling the record does not hold yet still honours the
        // recipe: it takes no key, and the recipe's own pair answers for
        // it, so the destination keeps only its anchor.
        let linked = dir(&root, "share/fw", &["/usr/share/fw", "/usr/share/link"]);
        let refs: Vec<&InferredDir> = vec![&linked];
        let claim = PathPair {
            absolute: "/usr/share/link".to_string(),
            relative: "share/other".to_string(),
        };
        let out = choose(&refs, &[], &lib, &root, &persisted, &[claim]);
        let got: Vec<(Mechanism, Vec<String>, Vec<String>)> = out
            .iter()
            .map(|r| (r.mechanism, r.keys.clone(), r.spellings.clone()))
            .collect();
        assert_eq!(
            got,
            vec![(
                Mechanism::Anchor,
                vec!["fw".to_string()],
                vec!["/usr/share/fw".to_string()]
            )]
        );
    }

    /// An earlier rewrite is found by the anchor root over the path's
    /// first bytes, as a whole string or before a separator, and not as
    /// the start of a longer name.
    #[test]
    fn an_earlier_rewrite_is_found_in_the_tree() {
        let root = temp_root("redir-rewritten-in");
        let file = |name: &str, string: &str| {
            let file = root.join(name);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(&file, crate::bundle::infer::test_elf(&[(READ, &[string])])).unwrap();
            file
        };
        let whole = file("whole.so", "/dev/fd/77/fw");
        let below = file("below.so", "/dev/fd/77/fw/a.conf");
        let longer = file("longer.so", "/dev/fd/77/fwx");
        assert!(rewritten_in(std::slice::from_ref(&whole), "/usr/share/fw").unwrap());
        assert!(rewritten_in(std::slice::from_ref(&below), "/usr/share/fw").unwrap());
        assert!(!rewritten_in(std::slice::from_ref(&longer), "/usr/share/fw").unwrap());
        // A path with a separator right after the overwritten bytes keeps
        // it in the rewrite, while its key leaves it out.
        let slashed = file("slashed.so", "/dev/fd/77//share/wfw");
        assert!(key_in_use(std::slice::from_ref(&slashed), "share/wfw").unwrap());
        assert!(rewritten_in(std::slice::from_ref(&slashed), "/var/tmp/fv/share/wfw").unwrap());
        assert!(!key_in_use(std::slice::from_ref(&slashed), "share/wf").unwrap());
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
        let (files, occurrences) = rewrite_anchors(&[anchored(&dir)], &[], &[file]).unwrap();
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
                source: None,
                dest: PathBuf::from("lib/kept"),
                mechanism: Mechanism::Env,
                keys: Vec::new(),
                spellings: Vec::new(),
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

    /// A mechanism resolves while the tree holds its bundled directory,
    /// and not when the tree lacks it.
    #[test]
    fn resolves_follows_the_tree() {
        let root = temp_root("redir-resolves");
        fs::create_dir_all(root.join("share/fw")).unwrap();
        assert!(anchor("a", "share/fw").resolves(&root));
        assert!(!anchor("a", "share/gone").resolves(&root));
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
        let (files, occurrences) =
            rewrite_anchors(&[anchored(&anchor_dir)], &[], std::slice::from_ref(&file)).unwrap();
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
                .any(|w| w == b"/dev/fd/77/fw\0"),
            "the rodata copy takes the anchor"
        );
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
        let (files, occurrences) =
            rewrite_anchors(&[anchored(&ro_dir)], &[], std::slice::from_ref(&file)).unwrap();
        assert_eq!((files, occurrences), (1, 1));
        let meta = fs::metadata(&file).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o500, "mode restored");
        let mtime = meta.modified().unwrap();
        let expected = crate::pack::source_date_epoch()
            .map(|s| std::time::UNIX_EPOCH + std::time::Duration::from_secs(s))
            .unwrap_or(std::time::UNIX_EPOCH);
        assert_eq!(mtime, expected, "mtime re-pinned");
        let rewritten = fs::read(&file).unwrap();
        assert!(rewritten.windows(14).any(|w| w == b"/dev/fd/77/fw\0"));
        let _ = fs::remove_dir_all(&root);
    }
    fn persisted_anchor(key: &str, relative: &str) -> anchors::Anchor {
        anchors::Anchor {
            key: key.to_string(),
            relative: relative.to_string(),
        }
    }

    /// The record a previous build left keeps its keys: its directory
    /// stays anchored with no fresh spelling at all, a fresh spelling
    /// that would make a recorded key name another directory takes a
    /// pair, and a key the record named for a directory the tree lost
    /// is not handed to a new one, whose bytes could still name it.
    #[test]
    fn a_recorded_key_keeps_its_directory() {
        let root = temp_root("redir-recorded");
        fs::create_dir_all(root.join("share/fw")).unwrap();
        let persisted = [
            persisted_anchor("fw", "share/fw"),
            persisted_anchor("gone", "share/gone"),
        ];
        let clash = dir(&root, "lib/fw", &["/usr/share/fw"]);
        let retired = dir(&root, "lib/gone", &["/usr/share/gone"]);
        let fresh = dir(&root, "lib/new", &["/usr/share/new"]);
        let refs: Vec<&InferredDir> = vec![&clash, &retired, &fresh];
        let out = choose(&refs, &[], &["lib".to_string()], &root, &persisted, &[]);
        let got: Vec<(&Path, Mechanism, Vec<String>)> = out
            .iter()
            .map(|r| (r.dest.as_path(), r.mechanism, r.keys.clone()))
            .collect();
        assert_eq!(
            got,
            vec![
                (Path::new("lib/fw"), Mechanism::Pair, vec![]),
                (Path::new("lib/gone"), Mechanism::Pair, vec![]),
                (
                    Path::new("lib/new"),
                    Mechanism::Anchor,
                    vec!["new".to_string()]
                ),
                (
                    Path::new("share/fw"),
                    Mechanism::Anchor,
                    vec!["fw".to_string()]
                ),
            ]
        );
    }

    /// A recorded directory that gains a spelling no key can carry keeps
    /// its anchor for the rest, and the new spelling alone takes a pair.
    #[test]
    fn a_recorded_directory_pairs_a_spelling_it_cannot_anchor() {
        let root = temp_root("redir-recorded-leftover");
        fs::create_dir_all(root.join("share/fw")).unwrap();
        let persisted = [persisted_anchor("fw", "share/fw")];
        let fw = dir(&root, "share/fw", &["/usr/share/fw", "/u/fw"]);
        let refs: Vec<&InferredDir> = vec![&fw];
        let out = choose(&refs, &[], &["lib".to_string()], &root, &persisted, &[]);
        let got: Vec<(Mechanism, Vec<String>, Vec<String>)> = out
            .iter()
            .map(|r| (r.mechanism, r.keys.clone(), r.spellings.clone()))
            .collect();
        assert_eq!(
            got,
            vec![
                (Mechanism::Pair, vec![], vec!["/u/fw".to_string()]),
                (
                    Mechanism::Anchor,
                    vec!["fw".to_string()],
                    vec!["/usr/share/fw".to_string()]
                ),
            ]
        );
        let pairs = record_pairs(&out, refs.iter().copied(), &[]);
        assert_eq!(
            pairs
                .iter()
                .map(|p| (p.absolute.as_str(), p.relative.as_str()))
                .collect::<Vec<_>>(),
            vec![("/u/fw", "share/fw")]
        );
        assert_eq!(
            descriptions(&out).get("share/fw").map(String::as_str),
            Some(
                "redirected by a path pair where the launch has a mount namespace, and \
                 redirected through /dev/fd/77/fw"
            )
        );
    }

    /// A key below another rides its link when it reaches the same
    /// directory the parent's link does, and takes a pair when it would
    /// need a directory of its own there.
    #[test]
    fn a_nested_key_must_agree_with_its_parent() {
        let root = temp_root("redir-nested");
        let parent = dir(&root, "share/alsa", &["/usr/share/alsa"]);
        let rider = dir(&root, "share/alsa/ucm2", &["/usr/share/alsa/ucm2"]);
        let stray = dir(&root, "share/elsewhere", &["/usr/share/alsa/other"]);
        let refs: Vec<&InferredDir> = vec![&parent, &rider, &stray];
        let out = choose(&refs, &[], &["lib".to_string()], &root, &[], &[]);
        let mechanism = |dest: &str| {
            out.iter()
                .find(|r| r.dest == Path::new(dest))
                .map(|r| r.mechanism)
                .unwrap()
        };
        assert_eq!(mechanism("share/alsa"), Mechanism::Anchor);
        assert_eq!(mechanism("share/alsa/ucm2"), Mechanism::Anchor);
        assert_eq!(mechanism("share/elsewhere"), Mechanism::Pair);
    }

    /// The record lists only the outermost keys: a key below another
    /// resolves through that one's link.
    #[test]
    fn the_record_holds_only_outermost_keys() {
        let root = temp_root("redir-record-nested");
        fs::create_dir_all(root.join("share/alsa/ucm2")).unwrap();
        fs::create_dir_all(root.join("share/xkb")).unwrap();
        let redirections = [
            anchor("alsa", "share/alsa"),
            anchor("alsa/ucm2", "share/alsa/ucm2"),
            Redirection {
                keys: vec!["X11/xkb".to_string(), "xkb".to_string()],
                ..anchor("unused", "share/xkb")
            },
        ];
        assert_eq!(write_record(&redirections, &root).unwrap(), 3);
        let bytes = fs::read(root.join(".onelf/anchors")).unwrap();
        assert_eq!(
            bytes,
            b"X11/xkb\tshare/xkb\nalsa\tshare/alsa\nxkb\tshare/xkb".to_vec()
        );
        let _ = fs::remove_dir_all(&root);
    }
}

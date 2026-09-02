//! The three tiers that take files out of a closure.
//!
//! A closure is complete by construction and therefore too large. The
//! platform line names what the host supplies; the policy names what never
//! ships; a trace, when the publisher has one, names what a test run did
//! not touch. Each tier is a plain text file so it can be published,
//! diffed and shared.

use std::collections::{BTreeSet, HashSet};
use std::io;
use std::path::Path;

/// Soname prefixes the host is expected to provide.
#[derive(Debug, Default, Clone)]
pub struct PlatformLine {
    prefixes: Vec<String>,
}

/// Glob patterns over paths relative to the root that never ship.
#[derive(Debug, Default)]
pub struct Policy {
    patterns: Vec<glob::Pattern>,
}

/// Paths a test run opened, relative to the root.
#[derive(Debug, Default)]
pub struct Trace {
    opened: BTreeSet<String>,
    opened_dirs: BTreeSet<String>,
}

/// What pruning kept and why the rest went.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    pub kept: Vec<String>,
    /// Basenames of the files the platform line removed.
    pub host_provided: Vec<String>,
    pub removed_platform: usize,
    pub removed_policy: usize,
    pub removed_trace: usize,
}

/// Non-empty, non-comment lines of a tier file.
fn read_lines(path: &Path) -> io::Result<Vec<String>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect())
}

impl PlatformLine {
    pub fn load(path: &Path) -> io::Result<Self> {
        Ok(Self::from_prefixes(read_lines(path)?))
    }

    pub fn from_prefixes(prefixes: Vec<String>) -> Self {
        PlatformLine { prefixes }
    }

    pub fn prefixes(&self) -> &[String] {
        &self.prefixes
    }

    /// Whether the file at `rel` is on the platform line, by basename.
    pub fn matches(&self, rel: &str) -> bool {
        let name = rel.rsplit('/').next().unwrap_or(rel);
        self.prefixes.iter().any(|p| name.starts_with(p.as_str()))
    }

    /// Whether `soname` is on the platform line.
    pub fn matches_soname(&self, soname: &str) -> bool {
        self.prefixes.iter().any(|p| soname.starts_with(p.as_str()))
    }
}

impl Policy {
    pub fn load(path: &Path) -> io::Result<Self> {
        let mut patterns = Vec::new();
        for line in read_lines(path)? {
            let pattern = glob::Pattern::new(line.trim_start_matches('/')).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {line}: {e}", path.display()),
                )
            })?;
            patterns.push(pattern);
        }
        Ok(Policy { patterns })
    }

    pub fn matches(&self, rel: &str) -> bool {
        let options = glob::MatchOptions {
            case_sensitive: true,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        };
        self.patterns.iter().any(|p| p.matches_with(rel, options))
    }
}

impl Trace {
    /// Read a trace: one path per line, absolute or relative to the root.
    pub fn load(path: &Path) -> io::Result<Self> {
        Ok(Self::from_paths(read_lines(path)?))
    }

    pub fn from_paths(paths: Vec<String>) -> Self {
        let mut trace = Trace::default();
        for p in paths {
            let rel = p.trim_start_matches('/').to_string();
            if let Some((dir, _)) = rel.rsplit_once('/') {
                trace.opened_dirs.insert(dir.to_string());
            }
            trace.opened.insert(rel);
        }
        trace
    }

    /// Whether `rel` survives: it was opened, or a file beside it was.
    fn keeps(&self, rel: &str) -> bool {
        if self.opened.contains(rel) {
            return true;
        }
        match rel.rsplit_once('/') {
            Some((dir, _)) => self.opened_dirs.contains(dir),
            None => false,
        }
    }
}

/// Apply the tiers to `files`, relative paths in any order. `keep_names`
/// are basenames that survive the trace regardless, the sonames some
/// bundled object needs.
pub fn prune(
    files: &[String],
    platform: Option<&PlatformLine>,
    policy: Option<&Policy>,
    trace: Option<&Trace>,
    keep_names: &HashSet<String>,
) -> Pruned {
    let mut out = Pruned::default();
    let mut sorted: Vec<&String> = files.iter().collect();
    sorted.sort();
    sorted.dedup();
    for rel in sorted {
        let name = rel.rsplit('/').next().unwrap_or(rel);
        if platform.is_some_and(|p| p.matches(rel)) {
            out.removed_platform += 1;
            out.host_provided.push(name.to_string());
            continue;
        }
        if policy.is_some_and(|p| p.matches(rel)) {
            out.removed_policy += 1;
            continue;
        }
        if let Some(trace) = trace
            && !trace.keeps(rel)
            && !keep_names.contains(name)
        {
            out.removed_trace += 1;
            continue;
        }
        out.kept.push(rel.clone());
    }
    out.host_provided.sort();
    out.host_provided.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::fixture::temp_root;

    fn files() -> Vec<String> {
        [
            "usr/bin/app",
            "usr/lib/libfixture.so.1",
            "usr/lib/libnvidia-glcore.so.550",
            "usr/lib/plugins/a.so",
            "usr/lib/plugins/b.so",
            "usr/share/doc/app/README",
            "usr/share/locale/de/LC_MESSAGES/app.mo",
            "usr/share/man/man1/app.1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn the_platform_line_removes_and_records() {
        let root = temp_root("platform");
        let path = root.join("platform-line.txt");
        std::fs::write(&path, "# host drivers\nlibnvidia\nlibGL.so\n").unwrap();
        let line = PlatformLine::load(&path).unwrap();
        let out = prune(&files(), Some(&line), None, None, &HashSet::new());
        assert_eq!(out.removed_platform, 1);
        assert_eq!(out.host_provided, ["libnvidia-glcore.so.550"]);
        assert!(!out.kept.iter().any(|f| f.contains("nvidia")));
        assert_eq!(out.kept.len(), files().len() - 1);
        assert!(line.matches_soname("libGL.so.1"));
    }

    #[test]
    fn the_policy_removes_documentation_and_nothing_else() {
        let root = temp_root("policy");
        let path = root.join("policy.txt");
        std::fs::write(&path, "usr/share/doc/**\n/usr/share/man/**\n").unwrap();
        let policy = Policy::load(&path).unwrap();
        let out = prune(&files(), None, Some(&policy), None, &HashSet::new());
        assert_eq!(out.removed_policy, 2);
        assert!(out.kept.iter().any(|f| f.ends_with("app.mo")));
        assert!(!out.kept.iter().any(|f| f.contains("share/doc")));
        assert!(!out.kept.iter().any(|f| f.contains("share/man")));
    }

    #[test]
    fn a_bad_pattern_names_the_file_and_line() {
        let root = temp_root("badpolicy");
        let path = root.join("policy.txt");
        std::fs::write(&path, "usr/[\n").unwrap();
        let err = Policy::load(&path).unwrap_err();
        assert!(err.to_string().contains("usr/["), "{err}");
    }

    #[test]
    fn the_trace_keeps_opened_files_their_siblings_and_needed_names() {
        let trace = Trace::from_paths(vec!["/usr/bin/app".into(), "/usr/lib/plugins/a.so".into()]);
        let keep: HashSet<String> = ["libfixture.so.1".to_string()].into_iter().collect();
        let out = prune(&files(), None, None, Some(&trace), &keep);
        assert!(out.kept.contains(&"usr/bin/app".to_string()));
        assert!(
            out.kept.contains(&"usr/lib/plugins/b.so".to_string()),
            "sibling kept"
        );
        assert!(
            out.kept.contains(&"usr/lib/libfixture.so.1".to_string()),
            "needed kept"
        );
        assert!(
            !out.kept.iter().any(|f| f.ends_with("app.mo")),
            "unopened removed"
        );
        assert_eq!(out.removed_trace, files().len() - 4);
    }

    #[test]
    fn no_trace_means_no_pruning() {
        let out = prune(&files(), None, None, None, &HashSet::new());
        assert_eq!(out.kept.len(), files().len());
        assert_eq!(out.removed_trace, 0);
    }
}

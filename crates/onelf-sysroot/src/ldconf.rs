//! The loader search directories a sysroot declares in `etc/ld.so.conf`.
//!
//! A package installed under `/opt` reaches its libraries through a
//! drop-in there rather than through `usr/lib`, so a closure that only
//! searches the standard directories reports them missing.

use std::path::{Path, PathBuf};

use crate::path::in_root;

/// The directories `etc/ld.so.conf` and its includes name, each resolved
/// inside `root`, existing ones only, in file order without duplicates.
pub fn search_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    if let Ok(conf) = in_root(root, Path::new("etc/ld.so.conf")) {
        read_conf(root, &conf, &mut out, &mut seen, 0);
    }
    out
}

fn read_conf(
    root: &Path,
    conf: &Path,
    out: &mut Vec<PathBuf>,
    seen: &mut std::collections::HashSet<PathBuf>,
    depth: usize,
) {
    if depth > 8 {
        return;
    }
    let Ok(text) = std::fs::read_to_string(conf) else {
        return;
    };
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(pattern) = line.strip_prefix("include ") {
            for file in expand(root, pattern.trim()) {
                read_conf(root, &file, out, seen, depth + 1);
            }
            continue;
        }
        let Ok(dir) = in_root(root, Path::new(line)) else {
            continue;
        };
        if dir.is_dir() && seen.insert(dir.clone()) {
            out.push(dir);
        }
    }
}

/// The files an `include` pattern matches inside `root`, sorted. Each
/// component is matched against the directory the ones before it
/// resolve to, so a glob never lists a directory outside the sysroot.
fn expand(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut found = vec![PathBuf::new()];
    for part in Path::new(pattern).iter() {
        let Some(part) = part.to_str() else {
            return Vec::new();
        };
        if part == "/" {
            continue;
        }
        if !part.contains(['*', '?', '[']) {
            for base in &mut found {
                base.push(part);
            }
            continue;
        }
        let Ok(glob) = glob::Pattern::new(part) else {
            return Vec::new();
        };
        found = found
            .iter()
            .flat_map(|base| {
                in_root(root, base)
                    .ok()
                    .and_then(|dir| std::fs::read_dir(dir).ok())
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.file_name())
                    .filter(|n| n.to_str().is_some_and(|n| glob.matches(n)))
                    .map(|n| base.join(n))
                    .collect::<Vec<_>>()
            })
            .collect();
    }
    let mut files: Vec<PathBuf> = found
        .iter()
        .filter_map(|p| in_root(root, p).ok())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::fixture::temp_root;

    #[test]
    fn includes_are_followed_and_missing_dirs_skipped() {
        let root = temp_root("ldconf");
        std::fs::create_dir_all(root.join("etc/ld.so.conf.d")).unwrap();
        std::fs::create_dir_all(root.join("opt/vendor/lib")).unwrap();
        std::fs::write(
            root.join("etc/ld.so.conf"),
            "# comment\ninclude /etc/ld.so.conf.d/*.conf\n/nonexistent\n",
        )
        .unwrap();
        std::fs::write(
            root.join("etc/ld.so.conf.d/vendor.conf"),
            "/opt/vendor/lib\n/opt/vendor/lib/\n",
        )
        .unwrap();
        assert_eq!(search_dirs(&root), vec![root.join("opt/vendor/lib")]);
        assert!(search_dirs(Path::new("/nonexistent-root")).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The configuration names the sysroot's directories, through
    /// whatever absolute links and `..` it holds, and never the packer's.
    #[test]
    fn directories_resolve_inside_the_sysroot() {
        let root = temp_root("ldconfroot");
        std::fs::create_dir_all(root.join("etc/conf.d")).unwrap();
        std::fs::create_dir_all(root.join("opt/vendor/lib")).unwrap();
        std::os::unix::fs::symlink("/opt/vendor", root.join("etc/vendor")).unwrap();
        std::os::unix::fs::symlink("/etc/conf.d", root.join("etc/ld.so.conf.d")).unwrap();
        std::fs::write(
            root.join("etc/ld.so.conf"),
            "include /etc/ld.so.conf.d/*.conf\n/../../../../../../../../tmp\n/usr\n",
        )
        .unwrap();
        std::fs::write(root.join("etc/conf.d/vendor.conf"), "/etc/vendor/lib\n").unwrap();
        assert_eq!(search_dirs(&root), vec![root.join("opt/vendor/lib")]);
    }
}

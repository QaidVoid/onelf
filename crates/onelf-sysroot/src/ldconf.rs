//! The loader search directories a sysroot declares in `etc/ld.so.conf`.
//!
//! A package installed under `/opt` reaches its libraries through a
//! drop-in there rather than through `usr/lib`, so a closure that only
//! searches the standard directories reports them missing.

use std::path::{Path, PathBuf};

/// The directories `etc/ld.so.conf` and its includes name, each mapped
/// under `root`, existing ones only, in file order without duplicates.
pub fn search_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    read_conf(root, &root.join("etc/ld.so.conf"), &mut out, &mut seen, 0);
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
            let pattern = root.join(pattern.trim().trim_start_matches('/'));
            let Ok(matches) = glob::glob(&pattern.to_string_lossy()) else {
                continue;
            };
            let mut files: Vec<PathBuf> = matches.flatten().collect();
            files.sort();
            for file in files {
                read_conf(root, &file, out, seen, depth + 1);
            }
            continue;
        }
        let dir = root.join(line.trim_start_matches('/'));
        if dir.is_dir() && seen.insert(dir.clone()) {
            out.push(dir);
        }
    }
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
}

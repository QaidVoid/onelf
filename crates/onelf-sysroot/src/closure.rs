//! The set of packages an application reaches.

use std::collections::{BTreeSet, VecDeque};

use crate::db::Database;

/// The packages reached from a root package, and the dependencies that
/// nothing in the sysroot satisfies.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Closure {
    /// Sorted package names, the root included.
    pub packages: Vec<String>,
    /// `(dependency, needed by)` for names no package provides. A
    /// debloated sysroot drops metapackages on purpose, so this is
    /// reported rather than fatal.
    pub unsatisfied: Vec<(String, String)>,
}

impl Database {
    /// The transitive `depends` closure of `root`, plus every optional
    /// dependency named in `optional`, resolved through `provides`.
    pub fn closure(&self, root: &str, optional: &[String]) -> Closure {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut unsatisfied = Vec::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        queue.push_back(root.to_string());
        while let Some(name) = queue.pop_front() {
            if !seen.insert(name.clone()) {
                continue;
            }
            let Some(package) = self.package(&name) else {
                continue;
            };
            let wanted = package.depends.iter().chain(
                package
                    .optdepends
                    .iter()
                    .filter(|o| optional.iter().any(|w| w == *o)),
            );
            for dep in wanted {
                match self.satisfier(dep) {
                    Some(p) => {
                        if !seen.contains(&p.name) {
                            queue.push_back(p.name.clone());
                        }
                    }
                    None => unsatisfied.push((dep.clone(), name.clone())),
                }
            }
        }
        unsatisfied.sort();
        unsatisfied.dedup();
        Closure {
            packages: seen.into_iter().collect(),
            unsatisfied,
        }
    }

    /// Every file owned by the packages in `closure`, sorted.
    pub fn files_of(&self, closure: &Closure) -> Vec<String> {
        let mut files: Vec<String> = closure
            .packages
            .iter()
            .filter_map(|n| self.package(n))
            .flat_map(|p| p.files.iter().cloned())
            .collect();
        files.sort();
        files.dedup();
        files
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::fixture::pkg;

    fn db() -> Database {
        let mut db = Database::default();
        let mut app = pkg("app", &["usr/bin/app"]);
        app.depends = vec!["libfixture".into(), "libvirtual.so".into()];
        app.optdepends = vec!["extra".into(), "absent".into()];
        db.insert(app);
        let mut lib = pkg("libfixture", &["usr/lib/libfixture.so.1"]);
        lib.depends = vec!["zlib".into()];
        db.insert(lib);
        db.insert(pkg("zlib", &["usr/lib/libz.so.1"]));
        let mut provider = pkg("provider", &["usr/lib/libvirtual.so.1"]);
        provider.provides = vec!["libvirtual.so".into()];
        db.insert(provider);
        db.insert(pkg("extra", &["usr/lib/plugins/extra.so"]));
        db.insert(pkg("unrelated", &["usr/bin/other"]));
        db
    }

    #[test]
    fn reaches_transitive_and_provided_dependencies() {
        let c = db().closure("app", &[]);
        assert_eq!(c.packages, ["app", "libfixture", "provider", "zlib"]);
        assert!(c.unsatisfied.is_empty());
        assert_eq!(
            db().files_of(&c),
            [
                "usr/bin/app",
                "usr/lib/libfixture.so.1",
                "usr/lib/libvirtual.so.1",
                "usr/lib/libz.so.1"
            ]
        );
    }

    #[test]
    fn optional_dependencies_enter_only_when_named() {
        let c = db().closure("app", &["extra".to_string()]);
        assert!(c.packages.contains(&"extra".to_string()));
        assert!(c.unsatisfied.is_empty());

        let c = db().closure("app", &["absent".to_string()]);
        assert_eq!(c.unsatisfied, [("absent".to_string(), "app".to_string())]);
    }

    #[test]
    fn a_missing_dependency_is_reported_not_fatal() {
        let mut db = db();
        let mut app = db.package("app").unwrap().clone();
        app.depends.push("gone".into());
        db.insert(app);
        let c = db.closure("app", &[]);
        assert!(c.packages.contains(&"zlib".to_string()));
        assert_eq!(c.unsatisfied, [("gone".to_string(), "app".to_string())]);
    }
}

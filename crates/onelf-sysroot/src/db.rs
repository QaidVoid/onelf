//! The pacman local database.
//!
//! Each installed package is a directory `<name>-<version>` under
//! `var/lib/pacman/local`, holding a `desc` file of `%SECTION%` blocks and
//! a `files` file listing what the package owns. Both are plain text.
//! Dependency names carry version constraints and soname suffixes
//! (`glibc>=2.39`, `libfoo.so=1-64`); only the name is kept, since the
//! sysroot is pinned and what is installed is what there is.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

/// One installed package.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub depends: Vec<String>,
    pub optdepends: Vec<String>,
    pub provides: Vec<String>,
    /// Owned files relative to the root, directories excluded.
    pub files: Vec<String>,
}

/// Every installed package, with indexes by provided name and by file.
#[derive(Debug, Default)]
pub struct Database {
    packages: BTreeMap<String, Package>,
    providers: BTreeMap<String, Vec<String>>,
    owners: BTreeMap<String, String>,
}

/// Where the database sits under a root.
pub const LOCAL_DB: &str = "var/lib/pacman/local";

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

impl Database {
    /// Read the database of the sysroot at `root`.
    pub fn read(root: &Path) -> io::Result<Self> {
        let dir = root.join(LOCAL_DB);
        let mut db = Database::default();
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", dir.display())))?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        entries.sort();
        for entry in entries {
            let package = read_package(&entry)?;
            db.insert(package);
        }
        Ok(db)
    }

    /// Add a package, indexing what it provides and owns. Later packages
    /// never displace an earlier owner of the same file.
    pub fn insert(&mut self, package: Package) {
        for provided in &package.provides {
            self.providers
                .entry(provided.clone())
                .or_default()
                .push(package.name.clone());
        }
        for file in &package.files {
            self.owners
                .entry(file.clone())
                .or_insert_with(|| package.name.clone());
        }
        self.packages.insert(package.name.clone(), package);
    }

    pub fn package(&self, name: &str) -> Option<&Package> {
        self.packages.get(name)
    }

    pub fn packages(&self) -> impl Iterator<Item = &Package> {
        self.packages.values()
    }

    /// The package owning `path`, given relative to the root.
    pub fn owner(&self, path: &str) -> Option<&Package> {
        let path = path.trim_start_matches('/');
        self.owners
            .get(path)
            .and_then(|name| self.packages.get(name))
    }

    /// The package satisfying a dependency name: the package itself, or
    /// the first provider of a virtual name.
    pub fn satisfier(&self, name: &str) -> Option<&Package> {
        if let Some(p) = self.packages.get(name) {
            return Some(p);
        }
        self.providers
            .get(name)
            .and_then(|list| list.iter().find_map(|n| self.packages.get(n)))
    }
}

fn read_package(dir: &Path) -> io::Result<Package> {
    let context = |what: &str| format!("{}: {what}", dir.display());
    let desc = std::fs::read_to_string(dir.join("desc"))
        .map_err(|e| io::Error::new(e.kind(), context(&format!("desc: {e}"))))?;
    let sections = parse_sections(&desc);
    let name = sections
        .get("NAME")
        .and_then(|v| v.first())
        .cloned()
        .ok_or_else(|| invalid(context("desc has no %NAME%")))?;
    let version = sections
        .get("VERSION")
        .and_then(|v| v.first())
        .cloned()
        .ok_or_else(|| invalid(context("desc has no %VERSION%")))?;
    let names = |key: &str| -> Vec<String> {
        sections
            .get(key)
            .map(|v| v.iter().map(|s| dependency_name(s)).collect())
            .unwrap_or_default()
    };

    let files_text = std::fs::read_to_string(dir.join("files"))
        .map_err(|e| io::Error::new(e.kind(), context(&format!("files: {e}"))))?;
    let files = parse_sections(&files_text)
        .remove("FILES")
        .unwrap_or_default()
        .into_iter()
        .filter(|f| !f.ends_with('/'))
        .collect();

    Ok(Package {
        name,
        version,
        depends: names("DEPENDS"),
        optdepends: names("OPTDEPENDS"),
        provides: names("PROVIDES"),
        files,
    })
}

/// `%KEY%` blocks, each holding the non-empty lines up to the next blank
/// line or the next key.
fn parse_sections(text: &str) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let line = line.trim_end();
        if line.len() > 2 && line.starts_with('%') && line.ends_with('%') {
            let key = line[1..line.len() - 1].to_string();
            out.entry(key.clone()).or_default();
            current = Some(key);
        } else if line.is_empty() {
            current = None;
        } else if let Some(key) = &current {
            out.get_mut(key).unwrap().push(line.to_string());
        }
    }
    out
}

/// The bare name of a dependency, provision or optional dependency entry:
/// `glibc>=2.39` is `glibc`, `libfoo.so=1-64` is `libfoo.so`, and
/// `mesa: OpenGL support` is `mesa`.
pub fn dependency_name(entry: &str) -> String {
    let end = entry.find(['<', '>', '=', ':']).unwrap_or(entry.len());
    entry[..end].trim().to_string()
}

#[cfg(test)]
pub(crate) mod fixture {
    use super::*;

    /// Write a package entry into the database under `root`.
    pub(crate) fn write_package(root: &Path, package: &Package, dirs: &[&str]) {
        let dir = root
            .join(LOCAL_DB)
            .join(format!("{}-{}", package.name, package.version));
        std::fs::create_dir_all(&dir).unwrap();
        let mut desc = format!(
            "%NAME%\n{}\n\n%VERSION%\n{}\n\n",
            package.name, package.version
        );
        for (key, list) in [
            ("DEPENDS", &package.depends),
            ("OPTDEPENDS", &package.optdepends),
            ("PROVIDES", &package.provides),
        ] {
            if !list.is_empty() {
                desc.push_str(&format!("%{key}%\n{}\n\n", list.join("\n")));
            }
        }
        std::fs::write(dir.join("desc"), desc).unwrap();
        let mut files = String::from("%FILES%\n");
        for d in dirs {
            files.push_str(d);
            files.push_str("/\n");
        }
        for f in &package.files {
            files.push_str(f);
            files.push('\n');
        }
        files.push_str("\n%BACKUP%\n");
        std::fs::write(dir.join("files"), files).unwrap();
    }

    pub(crate) fn temp_root(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "onelf-sysroot-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub(crate) fn pkg(name: &str, files: &[&str]) -> Package {
        Package {
            name: name.into(),
            version: "1.0-1".into(),
            files: files.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::*;
    use super::*;

    #[test]
    fn dependency_names_drop_constraints_and_descriptions() {
        assert_eq!(dependency_name("glibc>=2.39"), "glibc");
        assert_eq!(dependency_name("libfoo.so=1-64"), "libfoo.so");
        assert_eq!(dependency_name("mesa: OpenGL support"), "mesa");
        assert_eq!(dependency_name("zlib"), "zlib");
    }

    #[test]
    fn reads_packages_files_and_owners() {
        let root = temp_root("db");
        let mut app = pkg("app", &["usr/bin/app"]);
        app.depends = vec!["libfixture>=1".into(), "libfoo.so=1-64".into()];
        app.optdepends = vec!["extra: more".into()];
        write_package(&root, &app, &["usr", "usr/bin"]);
        let mut lib = pkg("libfixture", &["usr/lib/libfixture.so.1"]);
        lib.provides = vec!["libfoo.so=1-64".into()];
        write_package(&root, &lib, &["usr", "usr/lib"]);

        let db = Database::read(&root).unwrap();
        let app = db.package("app").unwrap();
        assert_eq!(app.depends, ["libfixture", "libfoo.so"]);
        assert_eq!(app.optdepends, ["extra"]);
        assert_eq!(db.owner("/usr/bin/app").unwrap().name, "app");
        assert_eq!(
            db.owner("usr/lib/libfixture.so.1").unwrap().name,
            "libfixture"
        );
        assert!(
            db.owner("usr/lib").is_none(),
            "directories are owned by no one"
        );
        assert_eq!(db.satisfier("libfoo.so").unwrap().name, "libfixture");
        assert!(db.satisfier("nothing").is_none());
    }

    #[test]
    fn a_truncated_desc_fails_naming_the_package() {
        let root = temp_root("dbcut");
        let dir = root.join(LOCAL_DB).join("broken-1.0-1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("desc"), "%NAME%\nbroken\n\n%VERSI").unwrap();
        std::fs::write(dir.join("files"), "%FILES%\n").unwrap();
        let err = Database::read(&root).unwrap_err();
        assert!(err.to_string().contains("broken-1.0-1"), "{err}");
    }

    #[test]
    fn a_missing_database_is_an_error() {
        let root = temp_root("nodb");
        assert!(Database::read(&root).is_err());
    }
}

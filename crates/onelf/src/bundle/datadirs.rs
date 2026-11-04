//! A warning for the plugin and data directories bundled libraries have
//! compiled in that the bundle does not answer for.
//!
//! A library that loads plugins or reads data from an absolute path built
//! into it reads the host's copy, or finds nothing, unless the runtime
//! points it elsewhere. The watched paths, the places that answer them
//! and the variables the runtime sets are derived from the catalog's
//! runtime path definitions (`onelf-format`'s `knowledge`), so a mapping
//! added there is linted here without a further list. The driver stack is
//! left out, since it comes from the host by design, and so are
//! directories the runtime adds to rather than replaces (GIO modules,
//! terminfo, locales), whose host copy is meant to stay reachable. GLib,
//! GTK, gdk-pixbuf and gobject-introspection join their directories at
//! run time, so no whole path is there to find.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use onelf_format::knowledge::{self, Place, Subject};
use onelf_format::rules::RuntimeHookId;

/// The watched subjects derived once from the built-in catalog.
fn components() -> &'static [Subject<'static>] {
    static COMPONENTS: OnceLock<Vec<Subject<'static>>> = OnceLock::new();
    COMPONENTS.get_or_init(knowledge::subjects)
}

/// A compiled-in path no copy in the bundle answers for.
#[derive(Debug, PartialEq)]
pub(crate) struct Finding {
    /// The file holding the path, relative to the tree.
    pub file: PathBuf,
    pub path: String,
    /// Where the bundle could carry the directory instead.
    pub carry: String,
    pub var: &'static str,
}

/// Every known compiled-in path in `elf_files` that the tree under `root`
/// carries no copy of. The findings are sorted by file, path and variable,
/// so the report is a function of the inputs alone and not of the order
/// the files were scanned in.
pub(crate) fn unanswered(root: &Path, elf_files: &[PathBuf]) -> Vec<Finding> {
    let lib_dirs = library_dirs(root, elf_files);
    let carried = |place: &Place| match place {
        Place::Library(rel) => lib_dirs.iter().any(|dir| dir.join(rel).exists()),
        Place::Root(rel) => root.join(rel).exists(),
    };
    let mut findings = Vec::new();
    for file in elf_files {
        let Ok(data) = fs::read(file) else {
            continue;
        };
        let rel = file.strip_prefix(root).unwrap_or(file).to_path_buf();
        let mut seen: Vec<&[u8]> = Vec::new();
        for piece in data.split(|&b| b == 0) {
            let Some(component) = component_of(piece, components()) else {
                continue;
            };
            if seen.contains(&piece) || component.places.iter().any(carried) {
                continue;
            }
            seen.push(piece);
            findings.push(Finding {
                file: rel.clone(),
                path: String::from_utf8_lossy(piece).into_owned(),
                carry: describe(&component.places),
                var: component.variable,
            });
        }
    }
    // GLib builds its module directory at run time, so no string names
    // it. A bundled GIO without modules of its own loads the host's, the
    // dconf settings backend and gvfs among them.
    let gio = elf_files.iter().find(|f| {
        f.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("libgio-2.0.so"))
    });
    if let Some(gio) = gio
        && let Some(subject) = knowledge::hook_subjects(RuntimeHookId::GioModulePaths).first()
        && !carried_module_dir(root, &lib_dirs, &subject.places)
    {
        findings.push(Finding {
            file: gio.strip_prefix(root).unwrap_or(gio).to_path_buf(),
            path: "the host's GIO modules (dconf, gvfs)".to_string(),
            carry: describe(&subject.places),
            var: subject.variable,
        });
    }
    findings.sort_by(|a, b| (&a.file, &a.path, a.var).cmp(&(&b.file, &b.path, b.var)));
    findings
}

/// Whether a subject's places hold a module directory in the tree.
fn carried_module_dir(root: &Path, lib_dirs: &[PathBuf], places: &[Place]) -> bool {
    places.iter().any(|place| match place {
        Place::Library(rel) => lib_dirs.iter().any(|dir| dir.join(rel).is_dir()),
        Place::Root(rel) => root.join(rel).is_dir(),
    })
}

/// The subject a compiled-in absolute path references, when one is known.
fn component_of<'a>(
    piece: &[u8],
    components: &'a [Subject<'static>],
) -> Option<&'a Subject<'static>> {
    if piece.first() != Some(&b'/')
        || piece.len() > 4096
        || !piece.iter().all(|b| b.is_ascii_graphic())
    {
        return None;
    }
    components
        .iter()
        .find(|c| c.endings.iter().any(|e| piece.ends_with(e.as_bytes())))
}

/// The directories of the tree holding a shared object, other than
/// `bin/`, `share/` and `etc/`: the ones `pack` records as library
/// directories.
fn library_dirs(root: &Path, elf_files: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for file in elf_files {
        let is_object = file
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.contains(".so"));
        let Some(dir) = file.parent().filter(|_| is_object) else {
            continue;
        };
        let top = dir
            .strip_prefix(root)
            .ok()
            .and_then(|r| r.components().next())
            .map(|c| c.as_os_str().to_string_lossy().into_owned());
        let excluded = top
            .as_deref()
            .is_none_or(|t| ["bin", "share", "etc"].contains(&t));
        if !excluded && !dirs.iter().any(|d| d == dir) {
            dirs.push(dir.to_path_buf());
        }
    }
    dirs
}

fn describe(places: &[Place]) -> String {
    places
        .iter()
        .map(|place| match place {
            Place::Library(rel) => format!("{rel} in a library directory"),
            Place::Root(rel) => (*rel).to_string(),
        })
        .collect::<Vec<_>>()
        .join(" or ")
}

/// Record `findings` as unchecked completeness findings: each names
/// the file holding the compiled-in path, and what the bundle would have
/// to carry, and the runtime set, for the path to stop reaching the
/// host. Without a copy these come from the host, built against the
/// host's libraries, or are missing there.
pub(crate) fn record(findings: &[Finding]) {
    for f in findings {
        super::completeness::record(super::Finding::new(
            f.file.display().to_string(),
            super::Outcome::Unchecked,
            format!(
                "reads {} from the host unless the bundle carries {} and the runtime sets {}",
                f.path, f.carry, f.var
            ),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use onelf_format::rules::ResourceKind;

    fn elf_with(pieces: &[&str]) -> Vec<u8> {
        let mut data = b"\x7fELF\0".to_vec();
        for piece in pieces {
            data.extend_from_slice(piece.as_bytes());
            data.push(0);
        }
        data
    }

    /// Create the subject's resource at `path`, with the kind the
    /// runtime's definitions declare for it.
    fn place_node(path: &Path, kind: ResourceKind) {
        match kind {
            ResourceKind::File => {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, "").unwrap();
            }
            ResourceKind::Directory => fs::create_dir_all(path).unwrap(),
        }
    }

    #[test]
    fn a_compiled_in_dir_is_reported_until_the_bundle_carries_it() {
        let root = std::env::temp_dir().join(format!("onelf-datadirs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let lib = root.join("lib");
        fs::create_dir_all(&lib).unwrap();
        let gst = lib.join("libgstreamer-1.0.so.0");
        fs::write(
            &gst,
            b"\x7fELF\0/usr/lib/x86_64-linux-gnu/gstreamer-1.0\0/usr/share/locale\0\
              /usr/share/alsa\0not /usr/lib/gstreamer-1.0\0",
        )
        .unwrap();
        fs::create_dir_all(root.join("share/alsa")).unwrap();
        let files = vec![gst.clone()];

        assert_eq!(
            unanswered(&root, &files),
            vec![Finding {
                file: PathBuf::from("lib/libgstreamer-1.0.so.0"),
                path: "/usr/lib/x86_64-linux-gnu/gstreamer-1.0".into(),
                carry: "gstreamer-1.0 in a library directory".into(),
                var: "GST_PLUGIN_SYSTEM_PATH_1_0",
            }]
        );

        fs::create_dir_all(lib.join("gstreamer-1.0")).unwrap();
        assert!(unanswered(&root, &files).is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_bundled_gio_without_modules_is_reported() {
        let root = std::env::temp_dir().join(format!("onelf-gio-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("lib")).unwrap();
        let gio = root.join("lib/libgio-2.0.so.0");
        fs::write(&gio, b"\x7fELF").unwrap();
        let files = vec![gio.clone()];
        let found = unanswered(&root, &files);
        assert!(found.iter().any(|f| f.var == "GIO_MODULE_DIR"), "{found:?}");
        fs::create_dir_all(root.join("lib/gio/modules")).unwrap();
        let found = unanswered(&root, &files);
        let _ = fs::remove_dir_all(&root);
        assert!(
            !found.iter().any(|f| f.var == "GIO_MODULE_DIR"),
            "{found:?}"
        );
    }

    /// Every alternative the catalog declares is reported exactly while
    /// the runtime would find no copy: a subject with nothing carried is
    /// reported, and carrying any one of its declared places silences it.
    #[test]
    fn a_carried_alternative_satisfies_the_lint_where_the_runtime_finds_it() {
        let mut subject_index = 0;
        for subject in knowledge::subjects() {
            for (place_index, place) in subject.places.iter().enumerate() {
                subject_index += 1;
                let root = std::env::temp_dir().join(format!(
                    "onelf-alt-{subject_index}-{place_index}-{}",
                    std::process::id()
                ));
                let _ = fs::remove_dir_all(&root);
                let lib = root.join("lib");
                fs::create_dir_all(&lib).unwrap();
                let probe = lib.join("libprobe.so");
                let piece = format!("/usr/lib/x86_64-linux-gnu{}", subject.endings[0]);
                fs::write(&probe, elf_with(&[&piece])).unwrap();
                let files = vec![probe];

                let found = unanswered(&root, &files);
                assert!(
                    found.iter().any(|f| f.var == subject.variable),
                    "{piece} unreported:\n{found:?}"
                );

                match place {
                    Place::Library(rel) => place_node(&lib.join(rel), subject.kind),
                    Place::Root(rel) => place_node(&root.join(rel), subject.kind),
                }
                let found = unanswered(&root, &files);
                assert!(
                    !found.iter().any(|f| f.var == subject.variable),
                    "{piece} reported though the alternative is carried:\n{found:?}"
                );

                let _ = fs::remove_dir_all(&root);
            }
        }
        assert!(subject_index > 0, "no subjects derived");
    }

    /// The compiled forms of a data path share one subject: a reference
    /// to the extension-stripped path is answered by the carried
    /// database, wherever the catalog names it.
    #[test]
    fn the_compiled_forms_of_a_data_path_are_answered_by_the_carried_copy() {
        let root = std::env::temp_dir().join(format!("onelf-magic-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let lib = root.join("lib");
        fs::create_dir_all(&lib).unwrap();
        let probe = lib.join("libprobe.so");
        fs::write(
            &probe,
            elf_with(&["/usr/share/misc/magic", "/usr/share/file/misc/magic"]),
        )
        .unwrap();
        let files = vec![probe];

        let found = unanswered(&root, &files);
        assert_eq!(
            found.len(),
            2,
            "both compiled forms are reported:\n{found:?}"
        );
        assert!(found.iter().all(|f| f.var == "MAGIC"), "{found:?}");

        place_node(&root.join("share/file/misc/magic.mgc"), ResourceKind::File);
        assert!(unanswered(&root, &files).is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    /// The findings' order is a function of their content, not of the
    /// scan: the same two probes handed over in either order come back
    /// sorted by file, so the report's bytes are stable.
    #[test]
    fn the_findings_are_sorted_regardless_of_scan_order() {
        let root = std::env::temp_dir().join(format!(
            "onelf-sort-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        let lib = root.join("lib");
        fs::create_dir_all(&lib).unwrap();
        let first = lib.join("libaaa.so");
        fs::write(
            &first,
            elf_with(&["/usr/lib/x86_64-linux-gnu/gstreamer-1.0"]),
        )
        .unwrap();
        let second = lib.join("libzzz.so");
        fs::write(&second, elf_with(&["/usr/share/misc/magic"])).unwrap();
        let files = vec![first.clone(), second.clone()];

        let forward = unanswered(&root, &files);
        assert_eq!(forward.len(), 2, "{forward:?}");
        assert_eq!(forward[0].file, PathBuf::from("lib/libaaa.so"));
        assert_eq!(forward[1].file, PathBuf::from("lib/libzzz.so"));
        assert_ne!(forward[0].var, forward[1].var);

        let reversed = unanswered(&root, &files.into_iter().rev().collect::<Vec<_>>());
        assert_eq!(
            reversed, forward,
            "scan order must not reorder the findings"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Every watched subject names an environment name the runtime's
    /// classification knows, so the lint's advice stays answerable.
    #[test]
    fn every_subject_names_a_classified_variable() {
        for subject in knowledge::subjects() {
            assert!(
                knowledge::classify(subject.variable).is_some(),
                "{} is unclassified",
                subject.variable
            );
            assert!(!subject.endings.is_empty(), "{}", subject.variable);
        }
    }
}

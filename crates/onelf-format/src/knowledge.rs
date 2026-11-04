//! Runtime knowledge derived from the built-in framework catalog.
//!
//! The missing-resource lint and the environment-name classification the
//! runtime needs both come from the catalog's runtime path definitions,
//! so a mapping added to a descriptor extends both without a further
//! list anywhere else. [`subjects`] names, for every mapped resource,
//! the compiled-in path endings that reference it and the places whose
//! carried copy answers them; [`classify`] and [`names`] describe how a
//! library reads the value of each mapped environment name.
//!
//! The registered runtime hooks and the launch's own procedures assign
//! further environment names outside the path groups. Their names are
//! fixed closed lists in [`crate::rules`], where descriptor validation
//! also refuses to map them, so every consumer classifies one set of names.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::OnceLock;

use crate::catalog;
pub use crate::rules::ValueShape;
use crate::rules::{
    Descriptor, HOOK_NAMES, PROCEDURE_NAMES, PathSelection, ResourceKind, RuntimeBase,
    RuntimeHookId, RuntimePathGroup, SourceBase,
};

/// Where the bundle carries a resource so the runtime's search finds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place<'a> {
    /// Below one of the bundle's library directories.
    Library(&'a str),
    /// Below the bundle's root.
    Root(&'a str),
}

/// One mapped resource the lint watches: the endings of compiled-in
/// paths that reference it, the places whose carried copy answers them,
/// and the environment name the runtime maps it through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subject<'a> {
    /// Compiled-in path endings, each beginning with `/`.
    pub endings: Vec<String>,
    /// The places carrying a copy answers every ending.
    pub places: Vec<Place<'a>>,
    /// The kind the resource must have where it is carried.
    pub kind: ResourceKind,
    /// The environment name the runtime maps the resource through.
    pub variable: &'a str,
}

/// The runtime classification of one environment name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NameClass {
    /// How the reading library splits the value.
    pub shape: ValueShape,
    /// The kind of path the name points at.
    pub kind: ResourceKind,
}

/// The names whose host copies stay reachable, which the lint leaves to
/// the runtime instead of asking the bundle to answer: the toolkit
/// module trees and the host's message catalogs.
const LINT_EXEMPT: &[&str] = &[
    "GI_TYPELIB_PATH",
    "GTK_PATH",
    "QT_PLUGIN_PATH",
    "TEXTDOMAINDIR",
];

/// One binding of a registered runtime hook to the descriptor group it
/// publishes, with the environment name that names the resource.
type HookBinding = (RuntimeHookId, &'static str, &'static str, &'static str);

/// The descriptor group each registered runtime hook publishes, with
/// the environment name that names it. The registry is closed, so this
/// is a fixed list.
const HOOK_BINDINGS: &[HookBinding] = &[
    (
        RuntimeHookId::GioModulePaths,
        "gio",
        "modules",
        "GIO_MODULE_DIR",
    ),
    (
        RuntimeHookId::TclTkLibraries,
        "tcl-8.6",
        "scripts",
        "TCL_LIBRARY",
    ),
    (
        RuntimeHookId::TclTkLibraries,
        "tcl-9.0",
        "scripts",
        "TCL_LIBRARY",
    ),
    (
        RuntimeHookId::TclTkLibraries,
        "tk-8.6",
        "scripts",
        "TK_LIBRARY",
    ),
    (
        RuntimeHookId::TclTkLibraries,
        "tk-9.0",
        "scripts",
        "TK_LIBRARY",
    ),
];

/// The missing-resource subjects the built-in catalog derives, in
/// catalog order.
///
/// A subject exists for every mapped resource the bundle can carry so
/// the runtime finds it. The names in [`LINT_EXEMPT`] have none, and
/// neither does the GIO module directory, whose path GLib builds at run
/// time; the hooks' carried resources join through their bindings.
pub fn subjects() -> Vec<Subject<'static>> {
    let mut out = subjects_in(catalog::builtins());
    out.extend(hook_subjects(RuntimeHookId::TclTkLibraries));
    out
}

/// The missing-resource subjects a set of descriptors derives.
pub fn subjects_in(descriptors: &[Descriptor]) -> Vec<Subject<'_>> {
    descriptors
        .iter()
        .flat_map(|descriptor| &descriptor.runtime.paths)
        .filter_map(group_subject)
        .collect()
}

/// The missing-resource subject one runtime path group derives, or
/// `None` when the lint leaves the group to the runtime.
pub fn group_subject(group: &RuntimePathGroup) -> Option<Subject<'_>> {
    if group
        .variables
        .iter()
        .any(|name| LINT_EXEMPT.contains(&name.as_str()))
    {
        return None;
    }
    let mut endings = Vec::new();
    for candidate in &group.candidates {
        push_endings(&mut endings, group.kind, candidate.path.as_str());
    }
    Some(Subject {
        endings,
        places: group
            .candidates
            .iter()
            .map(|candidate| runtime_place(candidate.base, candidate.path.as_str()))
            .collect(),
        kind: group.kind,
        variable: group.variables.first()?.as_str(),
    })
}

/// The subjects the registered runtime hooks publish through the
/// descriptor groups their bindings name.
pub fn hook_subjects(hook: RuntimeHookId) -> Vec<Subject<'static>> {
    let mut out = Vec::new();
    for (bound, descriptor_id, group_id, variable) in HOOK_BINDINGS {
        if *bound != hook {
            continue;
        }
        let group = catalog::builtins()
            .iter()
            .find(|descriptor| descriptor.id.as_str() == *descriptor_id)
            .and_then(|descriptor| {
                descriptor
                    .resources
                    .plugins
                    .iter()
                    .chain(&descriptor.resources.data)
                    .find(|group| group.id.as_str() == *group_id)
            });
        let Some(group) = group else {
            continue;
        };
        let mut endings = Vec::new();
        for candidate in &group.candidates {
            push_endings(&mut endings, group.kind, candidate.path.as_str());
        }
        out.push(Subject {
            endings,
            places: group
                .candidates
                .iter()
                .map(|candidate| source_place(candidate.base, candidate.path.as_str()))
                .collect(),
            kind: group.kind,
            variable,
        });
    }
    out
}

/// Every environment name the built-in definitions map, in sorted order.
pub fn names() -> impl Iterator<Item = &'static str> {
    classes().keys().copied()
}

/// The runtime classification of one environment name.
pub fn classify(name: &str) -> Option<NameClass> {
    classes().get(name).copied()
}

/// The classification a set of descriptors gives one of its mapped
/// names, hooks and procedures apart.
pub fn classify_in(descriptors: &[Descriptor], name: &str) -> Option<NameClass> {
    descriptors
        .iter()
        .flat_map(|descriptor| &descriptor.runtime.paths)
        .find(|group| group.variables.iter().any(|v| v.as_str() == name))
        .map(group_class)
}

/// The classification one runtime path group gives all its aliases.
///
/// A first-match group assigns one path; the other selections join one
/// path per root and so read as a list.
pub fn group_class(group: &RuntimePathGroup) -> NameClass {
    NameClass {
        shape: match group.selection {
            PathSelection::First => ValueShape::Single,
            PathSelection::All | PathSelection::FirstPerLibraryRoot => ValueShape::List,
        },
        kind: group.kind,
    }
}

/// The classification of every mapped name, built once.
fn classes() -> &'static BTreeMap<&'static str, NameClass> {
    static CLASSES: OnceLock<BTreeMap<&'static str, NameClass>> = OnceLock::new();
    CLASSES.get_or_init(|| {
        let mut out = BTreeMap::new();
        for descriptor in catalog::builtins() {
            for group in &descriptor.runtime.paths {
                let class = group_class(group);
                for name in &group.variables {
                    out.insert(name.as_str(), class);
                }
            }
        }
        for (_, names) in HOOK_NAMES.iter() {
            for (name, shape, kind) in names.iter() {
                out.insert(
                    name,
                    NameClass {
                        shape: *shape,
                        kind: *kind,
                    },
                );
            }
        }
        for (name, shape, kind) in PROCEDURE_NAMES.iter() {
            out.insert(
                name,
                NameClass {
                    shape: *shape,
                    kind: *kind,
                },
            );
        }
        out
    })
}

/// Where a runtime candidate is carried: a library-root candidate below
/// a library directory, a package-root candidate below the root.
fn runtime_place(base: RuntimeBase, path: &str) -> Place<'_> {
    match base {
        RuntimeBase::LibraryRoot => Place::Library(path),
        RuntimeBase::PackageRoot => Place::Root(path),
    }
}

/// Where a source candidate is carried: a library-directory candidate
/// below a library directory, an installation-prefix candidate below
/// the root.
fn source_place(base: SourceBase, path: &str) -> Place<'_> {
    match base {
        SourceBase::LibraryDir => Place::Library(path),
        SourceBase::InstallationPrefix => Place::Root(path),
    }
}

/// The compiled-in path endings that reference a candidate: the path
/// itself, and for a file also the path with its last extension
/// removed, which is how a library names a database its code reads with
/// the extension added (libmagic reads `magic.mgc` for a compiled-in
/// `magic`).
fn push_endings(out: &mut Vec<String>, kind: ResourceKind, path: &str) {
    let full = format!("/{path}");
    if !out.contains(&full) {
        out.push(full);
    }
    if kind != ResourceKind::File {
        return;
    }
    let source = Path::new(path);
    if let (Some(parent), Some(stem)) = (source.parent(), source.file_stem())
        && source.extension().is_some()
    {
        let stem = format!("/{}", parent.join(stem).to_string_lossy());
        if !out.contains(&stem) {
            out.push(stem);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{
        BuildRules, EnvironmentName, Inheritance, RelativePath, ResourceKind, Resources, RuleId,
        RuntimeCandidate, RuntimeRules, SCHEMA_VERSION,
    };

    fn descriptor(paths: Vec<RuntimePathGroup>) -> Descriptor {
        Descriptor {
            schema: SCHEMA_VERSION,
            id: RuleId::new("proof-framework").unwrap(),
            triggers: Vec::new(),
            resources: Resources::default(),
            build: BuildRules::default(),
            runtime: RuntimeRules {
                paths,
                hooks: Vec::new(),
            },
        }
    }

    fn group(id: &str, variable: &str, selection: PathSelection) -> RuntimePathGroup {
        RuntimePathGroup {
            id: RuleId::new(id).unwrap(),
            kind: ResourceKind::Directory,
            candidates: vec![RuntimeCandidate {
                base: RuntimeBase::LibraryRoot,
                path: RelativePath::new("proof/plugins").unwrap(),
            }],
            variables: vec![EnvironmentName::new(variable).unwrap()],
            selection,
            inheritance: Inheritance::Replace,
        }
    }

    /// A mapping added to the catalog extends both consumers through the
    /// same derivation, with no per-consumer list to edit.
    #[test]
    fn a_new_mapping_extends_both_consumers_without_further_tables() {
        let synthetic = group("proof", "PROOF_PLUGIN_PATH", PathSelection::All);

        let subject = group_subject(&synthetic).expect("the lint watches the new mapping");
        assert_eq!(subject.endings, ["/proof/plugins"]);
        assert_eq!(subject.places, [Place::Library("proof/plugins")]);
        assert_eq!(subject.kind, ResourceKind::Directory);
        assert_eq!(subject.variable, "PROOF_PLUGIN_PATH");

        assert_eq!(
            group_class(&synthetic),
            NameClass {
                shape: ValueShape::List,
                kind: ResourceKind::Directory,
            }
        );

        let descriptor = descriptor(vec![synthetic]);
        assert_eq!(subjects_in(std::slice::from_ref(&descriptor)).len(), 1);
        assert_eq!(
            classify_in(std::slice::from_ref(&descriptor), "PROOF_PLUGIN_PATH"),
            Some(NameClass {
                shape: ValueShape::List,
                kind: ResourceKind::Directory,
            })
        );
        assert_eq!(
            classify_in(std::slice::from_ref(&descriptor), "OTHER"),
            None
        );
    }

    /// Every binding resolves against the built-in catalog, so a rename
    /// there cannot silently drop a hook's subject.
    #[test]
    fn every_hook_binding_names_a_catalog_group() {
        for (hook, descriptor_id, group_id, variable) in HOOK_BINDINGS {
            let descriptor = catalog::builtins()
                .iter()
                .find(|descriptor| descriptor.id.as_str() == *descriptor_id)
                .unwrap_or_else(|| panic!("{descriptor_id} is missing"));
            assert!(
                descriptor
                    .resources
                    .plugins
                    .iter()
                    .chain(&descriptor.resources.data)
                    .any(|group| group.id.as_str() == *group_id),
                "{descriptor_id}/{group_id} is missing"
            );
            let subject = hook_subjects(*hook)
                .into_iter()
                .find(|subject| subject.variable == *variable)
                .unwrap_or_else(|| panic!("{variable} has no subject"));
            assert!(!subject.places.is_empty(), "{variable}");
        }
    }

    /// The classification follows the group definitions: first-match
    /// names read as one path, the other selections as lists, and the
    /// kind is the group's.
    #[test]
    fn classification_follows_the_group_definitions() {
        for descriptor in catalog::builtins() {
            for group in &descriptor.runtime.paths {
                let class = group_class(group);
                assert_eq!(class.kind, group.kind, "{}", group.id.as_str());
                assert_eq!(
                    class.shape == ValueShape::Single,
                    group.selection == PathSelection::First,
                    "{}",
                    group.variables[0].as_str()
                );
            }
        }
    }

    /// The names the launch's hooks and procedures assign keep their
    /// classification reachable.
    #[test]
    fn hook_and_procedure_names_are_classified() {
        assert_eq!(
            classify("GIO_MODULE_DIR"),
            Some(NameClass {
                shape: ValueShape::Single,
                kind: ResourceKind::Directory,
            })
        );
        assert_eq!(
            classify("GIO_EXTRA_MODULES"),
            Some(NameClass {
                shape: ValueShape::List,
                kind: ResourceKind::Directory,
            })
        );
        assert_eq!(
            classify("SSL_CERT_FILE"),
            Some(NameClass {
                shape: ValueShape::Single,
                kind: ResourceKind::File,
            })
        );
        assert!(classify("ONELF_UNMAPPED_NAME").is_none());
        for name in names() {
            assert!(classify(name).is_some(), "{name}");
        }
    }

    /// The lint derives no subject for the names whose host copies stay
    /// reachable, and a file candidate contributes its extension-stripped
    /// ending next to the full path.
    #[test]
    fn subjects_skip_exempt_names_and_add_file_stems() {
        for exempt in LINT_EXEMPT {
            assert!(
                catalog::builtins()
                    .iter()
                    .all(|descriptor| subjects_in(std::slice::from_ref(descriptor))
                        .iter()
                        .all(|subject| subject.variable != *exempt)),
                "{exempt} has a subject"
            );
        }
        let magic = subjects()
            .into_iter()
            .find(|subject| subject.variable == "MAGIC")
            .expect("MAGIC has a subject");
        assert_eq!(
            magic.endings,
            [
                "/share/file/misc/magic.mgc",
                "/share/file/misc/magic",
                "/share/misc/magic.mgc",
                "/share/misc/magic",
            ]
        );
        assert_eq!(
            magic.places,
            [
                Place::Root("share/file/misc/magic.mgc"),
                Place::Root("share/misc/magic.mgc"),
            ]
        );
    }
}

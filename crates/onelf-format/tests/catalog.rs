use onelf_format::catalog::builtins;
#[cfg(not(feature = "rules-toml"))]
use onelf_format::rules::*;
#[cfg(feature = "rules-toml")]
// The module binding resolves `crate::rules` inside the included build script.
use onelf_format::rules::{self, *};
use std::collections::BTreeSet;

#[cfg(feature = "rules-toml")]
// The catalog generation is exercised here; the script's `main` is not.
#[allow(dead_code)]
#[path = "../build.rs"]
mod catalog_build;

fn descriptor(id: &str) -> &'static Descriptor {
    builtins()
        .iter()
        .find(|rule| rule.id.as_str() == id)
        .unwrap()
}

fn mapping(variable: &str) -> &'static RuntimePathGroup {
    builtins()
        .iter()
        .flat_map(|rule| &rule.runtime.paths)
        .find(|path| path.variables.iter().any(|name| name.as_str() == variable))
        .unwrap()
}

#[test]
fn builtins_are_valid_unique_sorted_and_shared_without_a_parser() {
    validate_catalog(builtins(), &[], &[]).unwrap();
    assert!(builtins().windows(2).all(|pair| pair[0].id < pair[1].id));
    assert!(std::ptr::eq(builtins(), builtins()));
    let ids = builtins()
        .iter()
        .map(|rule| rule.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        ids.join("\n") + "\n",
        include_str!("fixtures/catalog/identities.txt")
    );
}

#[test]
fn runtime_mapping_golden_retains_variables_alternatives_and_policies() {
    let mut snapshot = String::new();
    for rule in builtins() {
        for path in &rule.runtime.paths {
            snapshot.push_str(&format!(
                "{}/{} {:?} {:?} {:?} {}\n",
                rule.id.as_str(),
                path.id.as_str(),
                path.kind,
                path.selection,
                path.inheritance,
                path.variables
                    .iter()
                    .map(EnvironmentName::as_str)
                    .collect::<Vec<_>>()
                    .join(","),
            ));
            for candidate in &path.candidates {
                snapshot.push_str(&format!(
                    "  {:?}:{}\n",
                    candidate.base,
                    candidate.path.as_str()
                ));
            }
        }
    }
    assert_eq!(snapshot, include_str!("fixtures/catalog/runtime-paths.txt"));
}

#[test]
fn existing_plugin_subdirectory_table_is_covered_without_flattening() {
    // The plugin trees the descriptors still carry, with the group
    // layouts a distribution installs. The trees the reduced
    // descriptors gave to inference keep their layouts by the pipeline
    // fixtures, and their runtime mappings below still name them.
    let expected = ["gio/modules", "gstreamer-1.0", "pipewire-0.3", "spa-0.2"];
    for subdirectory in expected {
        assert!(
            builtins()
                .iter()
                .flat_map(|rule| &rule.resources.plugins)
                .any(|group| {
                    let relative = group.destination.as_str().strip_prefix("lib/").unwrap();
                    subdirectory == relative || subdirectory.starts_with(&format!("{relative}/"))
                }),
            "missing preserved plugin tree: {subdirectory}"
        );
    }
    for (variable, relative) in [
        ("GST_PLUGIN_SYSTEM_PATH_1_0", "gstreamer-1.0"),
        ("GST_PLUGIN_SYSTEM_PATH", "gstreamer-1.0"),
        ("PIPEWIRE_MODULE_DIR", "pipewire-0.3"),
        ("SPA_PLUGIN_DIR", "spa-0.2"),
        ("GI_TYPELIB_PATH", "girepository-1.0"),
        ("BABL_PATH", "babl-0.1"),
        ("GEGL_PATH", "gegl-0.4"),
    ] {
        let path = mapping(variable);
        assert_eq!(path.kind, ResourceKind::Directory);
        assert_eq!(path.selection, PathSelection::All);
        assert_eq!(path.inheritance, Inheritance::Replace);
        assert_eq!(
            path.candidates,
            vec![RuntimeCandidate {
                base: RuntimeBase::LibraryRoot,
                path: RelativePath::new(relative).unwrap(),
            }]
        );
    }
}

#[test]
fn existing_library_data_and_runtime_data_tables_are_covered() {
    for (id, soname, paths, kind) in [
        (
            "alsa",
            "libasound.so.2",
            &["share/alsa"][..],
            ResourceKind::Directory,
        ),
        (
            "xkb",
            "libxkbcommon.so.0",
            &["share/X11/xkb", "share/xkeyboard-config-2"][..],
            ResourceKind::Directory,
        ),
        (
            "x11",
            "libX11.so.6",
            &["share/X11/locale"][..],
            ResourceKind::Directory,
        ),
        (
            "magic",
            "libmagic.so.1",
            &["share/file/misc/magic.mgc", "share/misc/magic.mgc"][..],
            ResourceKind::File,
        ),
    ] {
        let rule = descriptor(id);
        assert!(
            rule.triggers
                .iter()
                .any(|trigger| trigger.matches_soname(soname))
        );
        let data = &rule.resources.data[0];
        assert_eq!(data.kind, kind);
        assert_eq!(data.requirement, Requirement::Required);
        assert_eq!(
            data.candidates
                .iter()
                .map(|candidate| candidate.path.as_str())
                .collect::<Vec<_>>(),
            paths
        );
        assert!(
            data.candidates
                .iter()
                .all(|candidate| candidate.base == SourceBase::InstallationPrefix)
        );
    }
    for (variable, paths, kind) in [
        (
            "TEXTDOMAINDIR",
            &["share/locale"][..],
            ResourceKind::Directory,
        ),
        (
            "XLOCALEDIR",
            &["share/X11/locale"][..],
            ResourceKind::Directory,
        ),
        (
            "MAGIC",
            &["share/file/misc/magic.mgc", "share/misc/magic.mgc"][..],
            ResourceKind::File,
        ),
        (
            "XKB_CONFIG_ROOT",
            &["share/X11/xkb"][..],
            ResourceKind::Directory,
        ),
    ] {
        let path = mapping(variable);
        assert_eq!(path.kind, kind);
        assert_eq!(path.selection, PathSelection::First);
        assert_eq!(path.inheritance, Inheritance::Preserve);
        assert_eq!(
            path.candidates
                .iter()
                .map(|candidate| candidate.path.as_str())
                .collect::<Vec<_>>(),
            paths
        );
        assert!(
            path.candidates
                .iter()
                .all(|candidate| candidate.base == RuntimeBase::PackageRoot)
        );
    }
}

#[test]
fn scanner_is_required_only_for_its_optional_plugin_group_and_keeps_both_aliases() {
    let rule = descriptor("gstreamer");
    let helper = &rule.resources.helpers[0];
    let plugins = &rule.resources.plugins[0];
    assert_eq!(helper.group, plugins.id);
    assert_eq!(plugins.requirement, Requirement::Optional);
    assert_eq!(helper.requirement, Requirement::Required);
    assert_eq!(helper.kind, ResourceKind::File);
    assert_eq!(
        helper.destination.as_str(),
        "libexec/gstreamer-1.0/gst-plugin-scanner"
    );
    let expected = [
        "gstreamer-1.0/gst-plugin-scanner",
        "gstreamer1.0/gstreamer-1.0/gst-plugin-scanner",
        "libexec/gstreamer-1.0/gst-plugin-scanner",
    ];
    assert_eq!(
        helper
            .candidates
            .iter()
            .map(|candidate| candidate.path.as_str())
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        helper
            .candidates
            .iter()
            .map(|candidate| candidate.base)
            .collect::<Vec<_>>(),
        [
            SourceBase::LibraryDir,
            SourceBase::LibraryDir,
            SourceBase::InstallationPrefix
        ]
    );
    let runtime = mapping("GST_PLUGIN_SCANNER");
    assert_eq!(
        runtime
            .variables
            .iter()
            .map(EnvironmentName::as_str)
            .collect::<Vec<_>>(),
        ["GST_PLUGIN_SCANNER_1_0", "GST_PLUGIN_SCANNER"]
    );
    assert_eq!(runtime.kind, ResourceKind::File);
    assert_eq!(runtime.selection, PathSelection::First);
    assert_eq!(runtime.inheritance, Inheritance::Replace);
    assert_eq!(
        runtime
            .candidates
            .iter()
            .map(|candidate| candidate.path.as_str())
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        runtime
            .candidates
            .iter()
            .map(|candidate| candidate.base)
            .collect::<Vec<_>>(),
        [
            RuntimeBase::LibraryRoot,
            RuntimeBase::LibraryRoot,
            RuntimeBase::PackageRoot
        ]
    );
}

#[test]
fn heif_prefers_nested_plugins_for_each_library_root() {
    let path = mapping("LIBHEIF_PLUGIN_PATH");
    assert_eq!(path.selection, PathSelection::FirstPerLibraryRoot);
    assert_eq!(path.inheritance, Inheritance::Replace);
    assert_eq!(
        path.candidates
            .iter()
            .map(|candidate| candidate.path.as_str())
            .collect::<Vec<_>>(),
        ["libheif/plugins", "libheif"]
    );
    assert!(
        path.candidates
            .iter()
            .all(|candidate| candidate.base == RuntimeBase::LibraryRoot)
    );
}

#[test]
fn procedural_hooks_keep_phases_and_do_not_become_generic_assignments() {
    let build = builtins()
        .iter()
        .flat_map(|rule| {
            rule.build
                .hooks
                .iter()
                .map(move |hook| format!("{}:{hook:?}", rule.id.as_str()))
        })
        .collect::<Vec<_>>();
    assert_eq!(build, ["alsa:AlsaConfPatch"]);
    let runtime = builtins()
        .iter()
        .flat_map(|rule| {
            rule.runtime
                .hooks
                .iter()
                .map(move |hook| format!("{}:{hook:?}", rule.id.as_str()))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        runtime,
        [
            "alsa:AlsaConfigPathNaturalSort",
            "fontconfig:FontconfigGenerate",
            "gdk-pixbuf:GdkCacheAbsolutise",
            "gio:GioModulePaths",
            "tcl-tk:TclTkLibraries",
        ]
    );
    let procedural = [
        "ALSA_CONFIG_PATH",
        "FONTCONFIG_FILE",
        "GDK_PIXBUF_MODULE_FILE",
        "GTK_IM_MODULE_FILE",
        "GIO_MODULE_DIR",
        "GIO_EXTRA_MODULES",
        "TCL_LIBRARY",
        "TK_LIBRARY",
    ];
    let generic: BTreeSet<_> = builtins()
        .iter()
        .flat_map(|rule| &rule.runtime.paths)
        .flat_map(|path| &path.variables)
        .map(EnvironmentName::as_str)
        .collect();
    assert!(
        procedural
            .into_iter()
            .all(|variable| !generic.contains(variable))
    );
    assert!(
        builtins()
            .iter()
            .flat_map(|rule| &rule.resources.plugins)
            .all(|group| group.requirement == Requirement::Optional)
    );
    let fontconfig = descriptor("fontconfig");
    assert_eq!(fontconfig.resources, Resources::default());
}

#[test]
fn tcl_tk_versions_retain_required_scripts_and_library_prefix_alternatives() {
    for framework in ["tcl", "tk"] {
        for version in ["8.6", "9.0"] {
            let rule = descriptor(&format!("{framework}-{version}"));
            let scripts = &rule.resources.data[0];
            assert_eq!(scripts.requirement, Requirement::Required);
            assert_eq!(scripts.kind, ResourceKind::Directory);
            let library = format!("{framework}{version}");
            let share = format!("share/tcltk/{library}");
            assert_eq!(scripts.destination.as_str(), format!("lib/{library}"));
            assert_eq!(
                scripts.candidates,
                vec![
                    SourceCandidate {
                        base: SourceBase::LibraryDir,
                        path: RelativePath::new(&library).unwrap()
                    },
                    SourceCandidate {
                        base: SourceBase::InstallationPrefix,
                        path: RelativePath::new(&share).unwrap()
                    },
                ]
            );
        }
    }
    for (id, directory) in [
        ("gdk-pixbuf", "lib/gdk-pixbuf-2.0"),
        ("gtk3", "lib/gtk-3.0"),
    ] {
        assert_eq!(
            descriptor(id).resources.plugins[0].destination.as_str(),
            directory
        );
    }
}

/// The exclusions name the shared roots inside the resource bases, the
/// trees that hold every package's files or the host system's own below
/// every library directory or the prefix, glibc's own directories and the
/// driver stacks, and the generated table is shared without a parser.
#[test]
fn inference_exclusions_name_shared_roots_trees_libc_and_drivers() {
    let exclusions = onelf_format::catalog::inference_exclusions();
    exclusions.validate().unwrap();
    assert!(std::ptr::eq(
        exclusions,
        onelf_format::catalog::inference_exclusions()
    ));
    let roots: Vec<&str> = exclusions
        .shared_roots
        .iter()
        .map(RelativePath::as_str)
        .collect();
    assert_eq!(roots, ["share/X11", "share/xml"]);
    let names = |table: &[ExclusionEntry]| -> Vec<(ExclusionBase, String)> {
        table
            .iter()
            .map(|entry| (entry.base, entry.path.as_str().to_string()))
            .collect()
    };
    let trees = names(&exclusions.shared_trees);
    for tree in [
        "gcc", "go", "modules", "debug", "firmware", "udev", "pam.d", "security", "jvm",
    ] {
        assert!(
            trees.contains(&(ExclusionBase::LibraryDirs, tree.into())),
            "{tree} is a shared tree below every library directory"
        );
    }
    for tree in [
        "share/locale",
        "share/icons",
        "share/fonts",
        "share/doc",
        "share/man",
        "share/info",
        "share/applications",
        "share/mime",
        "share/pixmaps",
        "share/metainfo",
        "share/misc",
        "share/polkit-1",
    ] {
        assert!(
            trees.contains(&(ExclusionBase::InstallationPrefix, tree.into())),
            "{tree} is a shared tree"
        );
    }
    assert!(
        !trees
            .iter()
            .any(|(_, t)| std::path::Path::new(t).starts_with("share/X11")),
        "share/X11/xkb and share/X11/locale stay collectable"
    );
    let libc = names(&exclusions.libc);
    for dir in ["gconv", "getconf", "locale"] {
        assert!(
            libc.contains(&(ExclusionBase::LibraryDirs, dir.into())),
            "{dir}"
        );
    }
    assert!(libc.contains(&(ExclusionBase::InstallationPrefix, "share/zoneinfo".into())));
    let drivers = names(&exclusions.drivers);
    for dir in ["dri", "gbm", "vdpau"] {
        assert!(
            drivers.contains(&(ExclusionBase::LibraryDirs, dir.into())),
            "{dir}"
        );
    }
    for dir in ["share/drirc.d", "share/libdrm", "share/nvidia"] {
        assert!(
            drivers.contains(&(ExclusionBase::InstallationPrefix, dir.into())),
            "{dir}"
        );
    }
    let lib_relative = exclusions
        .shared_trees
        .iter()
        .chain(&exclusions.libc)
        .chain(&exclusions.drivers)
        .filter(|entry| entry.base == ExclusionBase::InstallationPrefix)
        .find(|entry| !entry.path.as_str().starts_with("share/"));
    assert!(
        lib_relative.is_none(),
        "a library directory entry is lib-dirs relative: {lib_relative:?}"
    );
}

#[cfg(feature = "rules-toml")]
mod generation {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "onelf-catalog-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn sources() -> Vec<PathBuf> {
        catalog_build::catalog_sources(&Path::new(env!("CARGO_MANIFEST_DIR")).join("rules"))
            .unwrap()
    }

    #[test]
    fn generated_typed_definitions_equal_every_parsed_canonical_source() {
        let mut parsed = sources()
            .into_iter()
            .map(|source| {
                onelf_format::rules_toml::parse_descriptor(&fs::read_to_string(source).unwrap())
                    .unwrap()
            })
            .collect::<Vec<_>>();
        parsed.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(builtins(), parsed);
        assert_eq!(
            catalog_build::generate_catalog(sources()).unwrap(),
            include_str!(concat!(env!("OUT_DIR"), "/framework_catalog.rs"))
        );
    }

    #[test]
    fn generation_is_independent_of_enumeration_and_filename_order() {
        let sources = sources();
        let expected = catalog_build::generate_catalog(sources.clone()).unwrap();
        assert_eq!(
            expected,
            catalog_build::generate_catalog(sources.iter().rev().cloned()).unwrap()
        );
        for reverse in [false, true] {
            let directory = TemporaryDirectory::new();
            let mut indices: Vec<_> = (0..sources.len()).collect();
            if reverse {
                indices.reverse();
            }
            for index in indices {
                let name = format!("{:03}.toml", sources.len() - index);
                fs::copy(&sources[index], directory.0.join(name)).unwrap();
            }
            fs::write(directory.0.join("ignored.txt"), "not a descriptor").unwrap();
            fs::create_dir(directory.0.join("ignored.toml")).unwrap();
            let discovered = catalog_build::catalog_sources(&directory.0).unwrap();
            assert!(discovered.windows(2).all(|pair| pair[0] < pair[1]));
            assert_eq!(
                expected,
                catalog_build::generate_catalog(discovered).unwrap()
            );
        }
    }

    #[test]
    fn generation_rejects_invalid_sources_and_duplicate_identities() {
        let directory = TemporaryDirectory::new();
        assert!(catalog_build::catalog_sources(&directory.0).is_err());
        let first = directory.0.join("first.toml");
        let second = directory.0.join("second.toml");
        fs::write(&first, "schema = 1\nid = 'same'\n").unwrap();
        fs::write(&second, "schema = 1\nid = 'same'\n").unwrap();
        let error = catalog_build::generate_catalog([first.clone(), second.clone()])
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("duplicate") && error.contains("same"),
            "{error}"
        );
        fs::write(&second, "schema = 2\nid = 'invalid'\n").unwrap();
        let error = catalog_build::generate_catalog([first, second])
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("second.toml") && error.contains("unsupported version"),
            "{error}"
        );
    }

    #[test]
    fn generated_exclusions_equal_the_parsed_source() {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join(catalog_build::EXCLUSIONS_SOURCE);
        let parsed = onelf_format::rules_toml::parse_inference_exclusions(
            &fs::read_to_string(&source).unwrap(),
        )
        .unwrap();
        assert_eq!(onelf_format::catalog::inference_exclusions(), &parsed);
        assert_eq!(
            catalog_build::generate_exclusions(&source).unwrap(),
            include_str!(concat!(env!("OUT_DIR"), "/inference_exclusions.rs"))
        );
        assert!(
            !sources().contains(&source),
            "the exclusions are not a descriptor source"
        );
    }

    #[test]
    fn exclusions_reject_unknown_fields_repeats_and_an_empty_root_table() {
        let parse = onelf_format::rules_toml::parse_inference_exclusions;
        let error = parse("shared-roots = [\"share/X11\"]\nextra = 1\n").unwrap_err();
        assert!(error.to_string().contains("unknown field"), "{error}");
        let error = parse("shared-roots = [\"share/X11\", \"./share/X11\"]\n").unwrap_err();
        assert!(error.to_string().contains("duplicate"), "{error}");
        let error = parse(
            "shared-roots = [\"share/X11\"]\n\
             shared-trees = [{ base = \"lib-dirs\", path = \"gcc\" }]\n\
             libc = [{ base = \"lib-dirs\", path = \"gcc\" }]\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("duplicate"), "{error}");
        let error =
            parse("shared-roots = [\"share/X11\"]\nshared-trees = [\"share/doc\"]\n").unwrap_err();
        assert!(error.to_string().contains("expected a table"), "{error}");
        let error = parse(
            "shared-roots = [\"share/X11\"]\nlibc = [{ base = \"lib-dirs\", path = \"dri\" }]\n\
             drivers = [{ base = \"lib-dirs\", path = \"dri/\" }]\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("duplicate"), "{error}");
        let error = parse(
            "shared-roots = [\"share/X11\"]\n\
             shared-trees = [{ base = \"installation-prefix\", path = \"share/X11\" }]\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("also names"), "{error}");
        let error = parse(
            "shared-roots = [\"share/X11\"]\n\
             drivers = [{ base = \"library-dir\", path = \"dri\" }]\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("unknown variant"), "{error}");
        let error = parse(
            "shared-roots = [\"share/X11\"]\n\
             drivers = [{ base = \"lib-dirs\", path = \"x86_64-linux-gnu/dri\" }]\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("multiarch"), "{error}");
        let error = parse("libc = []\n").unwrap_err();
        assert!(error.to_string().contains("at least one root"), "{error}");
        let error = parse("shared-roots = [\"/usr/lib\"]\n").unwrap_err();
        assert!(
            error.to_string().contains("confined relative path"),
            "{error}"
        );
        let error = parse("shared-roots = [\n").unwrap_err();
        assert!(error.to_string().contains("TOML line"), "{error}");
        let oversized = format!(
            "# {}\nshared-roots = [\"share/X11\"]\n",
            "x".repeat(1024 * 1024)
        );
        let error = parse(&oversized).unwrap_err().to_string();
        assert!(
            error.contains("byte limit") && !error.contains("DESCRIPTOR"),
            "{error}"
        );
    }
}

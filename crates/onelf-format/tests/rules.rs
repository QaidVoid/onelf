use onelf_format::rules::*;

fn id(value: &str) -> RuleId {
    RuleId::new(value).unwrap()
}

fn path(value: &str) -> RelativePath {
    RelativePath::new(value).unwrap()
}

fn descriptor(name: &str) -> Descriptor {
    Descriptor {
        schema: SCHEMA_VERSION,
        id: id(name),
        triggers: Vec::new(),
        resources: Resources::default(),
        build: BuildRules::default(),
        runtime: RuntimeRules::default(),
    }
}

fn plugin(name: &str) -> ResourceGroup {
    ResourceGroup {
        id: id(name),
        kind: ResourceKind::Directory,
        candidates: vec![SourceCandidate {
            base: SourceBase::LibraryDir,
            path: path("example/plugins"),
        }],
        destination: path("lib/example/plugins"),
        requirement: Requirement::Optional,
        members: Vec::new(),
    }
}

fn runtime_path(name: &str) -> RuntimePathGroup {
    RuntimePathGroup {
        id: id(name),
        kind: ResourceKind::Directory,
        candidates: vec![RuntimeCandidate {
            base: RuntimeBase::LibraryRoot,
            path: path("example/plugins"),
        }],
        variables: vec![EnvironmentName::new("EXAMPLE_PLUGIN_PATH").unwrap()],
        selection: PathSelection::All,
        inheritance: Inheritance::Replace,
    }
}

fn error_contains<T: std::fmt::Debug>(result: Result<T, RuleError>, expected: &str) {
    let error = result.unwrap_err().to_string();
    assert!(
        error.contains(expected),
        "expected {expected:?}, got {error:?}"
    );
}

#[test]
fn default_collections_and_direct_construction_need_no_parser() {
    let rule = descriptor("example");
    assert_eq!(rule.resources, Resources::default());
    assert_eq!(rule.build, BuildRules::default());
    assert_eq!(rule.runtime, RuntimeRules::default());
    rule.validate().unwrap();
    validate_catalog(&[rule], &[], &[]).unwrap();
}

#[test]
fn identity_and_environment_names_are_bounded_and_portable() {
    for name in ["a", "Example-1.0_name", &"a".repeat(MAX_ID_BYTES)] {
        assert_eq!(id(name).as_str(), name);
    }
    for name in ["", "-a", ".", "..", "foo/bar", "foo bar", "foo\0bar", "a=b"] {
        assert!(RuleId::new(name).is_err(), "{name:?}");
    }
    error_contains(RuleId::new(&"a".repeat(MAX_ID_BYTES + 1)), "MAX_ID_BYTES");
    for name in [
        "_",
        "XKB_CONFIG_ROOT",
        "lower_case_1",
        &"A".repeat(MAX_ID_BYTES),
    ] {
        assert_eq!(EnvironmentName::new(name).unwrap().as_str(), name);
    }
    for name in ["", "1NAME", "A=B", "A-B", "A B", "A\0B", "A\nB"] {
        assert!(EnvironmentName::new(name).is_err(), "{name:?}");
    }
    error_contains(
        EnvironmentName::new(&"A".repeat(MAX_ID_BYTES + 1)),
        "MAX_ID_BYTES",
    );
    for name in [
        "LD_LIBRARY_PATH",
        "LD_LIBRARY_PATH_32",
        "LD_LIBRARY_PATH_64",
        "LD_AOUT_LIBRARY_PATH",
        "LD_PRELOAD",
        "LD_AUDIT",
        "LD_BIND_NOW",
        "GCONV_PATH",
        "GLIBC_TUNABLES",
    ] {
        error_contains(EnvironmentName::new(name), "loader-control");
    }
    for name in ["ONELF_DIR", "ONELF_MODE", "ONELF_ANYTHING"] {
        error_contains(EnvironmentName::new(name), "runtime-owned");
    }
    for name in ["LDFLAGS", "XLD_PRELOAD", "ONELF", "MY_ONELF_PATH"] {
        assert!(EnvironmentName::new(name).is_ok(), "{name}");
    }
}

#[test]
fn hook_and_procedure_names_cannot_be_mapped() {
    for name in [
        "FONTCONFIG_FILE",
        "GIO_MODULE_DIR",
        "TCL_LIBRARY",
        "LOCPATH",
    ] {
        let mut group = runtime_path("owned");
        group.variables = vec![EnvironmentName::new(name).unwrap()];
        let rules = RuntimeRules {
            paths: vec![group],
            hooks: Vec::new(),
        };
        error_contains(rules.validate(), "runtime hook or launch procedure");
        let mut rule = descriptor("owned");
        rule.runtime = rules;
        error_contains(rule.validate(), "runtime hook or launch procedure");
    }
}

#[test]
fn one_variable_cannot_be_mapped_by_two_enabled_groups() {
    let mut first = descriptor("first");
    first.runtime.paths = vec![runtime_path("plugins")];
    let mut second = descriptor("second");
    second.runtime.paths = vec![runtime_path("plugins")];
    error_contains(
        validate_catalog(
            std::slice::from_ref(&first),
            std::slice::from_ref(&second),
            &[],
        ),
        "more than one enabled runtime path group",
    );
    validate_catalog(
        std::slice::from_ref(&first),
        std::slice::from_ref(&second),
        &[id("first")],
    )
    .unwrap();
    let mut both = descriptor("both");
    both.runtime.paths = vec![runtime_path("a"), runtime_path("b")];
    both.validate().unwrap();
    error_contains(
        validate_catalog(&[both], &[], &[]),
        "more than one enabled runtime path group",
    );
}

#[test]
fn set_names_are_identities_without_dots() {
    for name in ["gtk", "qt6", "A-b_1"] {
        assert_eq!(RuleId::set_name(name).unwrap().as_str(), name);
    }
    for name in ["", "qt6.core", "-x", "_x", "a/b", "a b"] {
        assert!(RuleId::set_name(name).is_err(), "{name:?}");
    }
}

#[test]
fn paths_normalize_dots_without_allowing_escape_or_platform_prefixes() {
    assert_eq!(
        path("./share//alsa/./alsa.conf/").as_str(),
        "share/alsa/alsa.conf"
    );
    assert_eq!(
        path(&"a".repeat(MAX_PATH_BYTES)).as_str().len(),
        MAX_PATH_BYTES
    );
    for unsafe_path in [
        "",
        ".",
        "./",
        "/",
        "/usr/lib",
        "../lib",
        "a/../lib",
        "a/..",
        "a\0b",
        "a\nb",
        "a\\b",
        "C:/lib",
        "C:lib",
        "./C:/lib",
        "\\\\server\\share",
    ] {
        assert!(RelativePath::new(unsafe_path).is_err(), "{unsafe_path:?}");
    }
    error_contains(
        RelativePath::new(&"a".repeat(MAX_PATH_BYTES + 1)),
        "MAX_PATH_BYTES",
    );
}

#[test]
fn triggers_match_only_literal_case_sensitive_soname_prefixes() {
    let trigger = Trigger {
        soname_prefix: "libexample.so".into(),
    };
    assert!(trigger.matches_soname("libexample.so.2"));
    for value in [
        "other-libexample.so.2",
        "Libexample.so.2",
        "/usr/lib/libexample.so.2",
        "libexample.so/*",
        "libexample.so\0.2",
        "",
    ] {
        assert!(!trigger.matches_soname(value), "{value:?}");
    }
    for prefix in [
        "", ".", "..", "lib*.so", "lib?.so", "[a]", "lib/foo", "lib\\foo", "lib\0foo",
    ] {
        let mut rule = descriptor("test");
        rule.triggers.push(Trigger {
            soname_prefix: prefix.into(),
        });
        error_contains(rule.validate(), "soname-prefix");
    }
    let mut rule = descriptor("test");
    rule.triggers.push(Trigger {
        soname_prefix: "a".repeat(MAX_PATH_BYTES + 1),
    });
    error_contains(rule.validate(), "soname-prefix");
}

#[test]
fn parsed_soname_suffixes_are_not_interpreted_as_patterns() {
    let trigger = Trigger {
        soname_prefix: "libasound.so".into(),
    };
    for soname in [
        "libasound.so.2[debug]",
        "libasound.so.2*debug",
        "libasound.so.2?debug",
        "libasound.so.2:debug",
    ] {
        assert!(trigger.matches_soname(soname), "{soname:?}");
    }
    assert!(!trigger.matches_soname("other-libasound.so.2[debug]"));
    assert!(!trigger.matches_soname("/usr/lib/libasound.so.2[debug]"));
}

#[test]
fn catalog_rejects_shadowing_even_after_disable_and_unknown_disabled_ids() {
    let builtin = descriptor("builtin");
    let local = descriptor("local");
    validate_catalog(
        std::slice::from_ref(&builtin),
        std::slice::from_ref(&local),
        &[id("builtin"), id("local")],
    )
    .unwrap();
    error_contains(validate_catalog(&[], &[local.clone(), local], &[]), "local");
    error_contains(
        validate_catalog(
            std::slice::from_ref(&builtin),
            std::slice::from_ref(&builtin),
            &[id("builtin")],
        ),
        "builtin",
    );
    error_contains(
        validate_catalog(&[builtin.clone(), builtin.clone()], &[], &[]),
        "duplicate",
    );
    error_contains(
        validate_catalog(std::slice::from_ref(&builtin), &[], &[id("missing")]),
        "missing",
    );
    error_contains(
        validate_catalog(&[builtin], &[], &[id("builtin"), id("builtin")]),
        "duplicate",
    );
}

#[test]
fn duplicate_nested_identities_are_rejected_within_their_namespaces() {
    let mut rule = descriptor("test");
    rule.resources.plugins.push(plugin("plugins"));
    rule.resources.data.push(plugin("plugins"));
    error_contains(rule.validate(), "resource id");
    rule.resources.data.clear();
    rule.runtime.paths.push(runtime_path("plugins"));
    rule.validate().unwrap();
    rule.runtime.paths.push(runtime_path("plugins"));
    error_contains(rule.validate(), "runtime path id");
    rule.runtime.paths.pop();
    rule.runtime.paths[0]
        .variables
        .push(EnvironmentName::new("EXAMPLE_PLUGIN_PATH").unwrap());
    error_contains(rule.validate(), "environment alias");
    rule.runtime.paths.clear();
    rule.build.hooks = vec![BuildHookId::AlsaConfPatch; 2];
    error_contains(rule.validate(), "build hook");
    rule.build.hooks.clear();
    rule.runtime.hooks = vec![RuntimeHookId::FontconfigGenerate; 2];
    error_contains(rule.validate(), "runtime hook");
    rule.runtime.hooks.clear();
    rule.triggers = vec![
        Trigger {
            soname_prefix: "libexample.so".into()
        };
        2
    ];
    error_contains(rule.validate(), "soname-prefix");
}

#[test]
fn resource_kinds_references_and_candidate_policies_are_validated() {
    let mut rule = descriptor("test");
    rule.resources.plugins.push(plugin("plugins"));
    rule.resources.plugins[0].kind = ResourceKind::File;
    error_contains(rule.validate(), "plugins.kind");
    rule.resources.plugins[0].kind = ResourceKind::Directory;
    rule.resources.helpers.push(HelperGroup {
        id: id("helper"),
        group: id("plugins"),
        kind: ResourceKind::File,
        candidates: vec![SourceCandidate {
            base: SourceBase::InstallationPrefix,
            path: path("libexec/helper"),
        }],
        destination: path("libexec/helper"),
        requirement: Requirement::Required,
    });
    rule.validate().unwrap();
    rule.resources.helpers[0].kind = ResourceKind::Directory;
    error_contains(rule.validate(), "helpers.kind");
    rule.resources.helpers[0].kind = ResourceKind::File;
    rule.resources.helpers[0].group = id("helper");
    error_contains(rule.validate(), "unknown plugin or data group");
    rule.resources.helpers[0].group = id("missing");
    error_contains(rule.validate(), "missing");
    rule.resources.helpers[0].group = id("plugins");
    rule.resources.helpers[0].id = id("plugins");
    error_contains(rule.validate(), "resource id");
    rule.resources.helpers.clear();
    rule.runtime.paths.push(runtime_path("plugins"));
    rule.runtime.paths[0].selection = PathSelection::FirstPerLibraryRoot;
    rule.validate().unwrap();
    rule.runtime.paths[0].candidates[0].base = RuntimeBase::PackageRoot;
    error_contains(rule.validate(), "requires library-root");
    rule.runtime.paths[0].selection = PathSelection::First;
    rule.runtime.paths[0].variables.clear();
    error_contains(rule.validate(), "at least one environment alias");
    rule.runtime.paths[0] = runtime_path("plugins");
    rule.runtime.paths[0].candidates.clear();
    error_contains(rule.validate(), "at least one source or runtime candidate");
    rule.runtime.paths.clear();
    rule.resources.plugins[0].candidates.clear();
    error_contains(rule.validate(), "at least one source or runtime candidate");
}

#[test]
fn aggregate_model_records_are_checked_before_identity_indexing() {
    let mut rule = descriptor("test");
    rule.triggers = (0..MAX_RECORDS - 1)
        .map(|i| Trigger {
            soname_prefix: format!("lib{i}.so"),
        })
        .collect();
    rule.validate().unwrap();
    rule.triggers.push(Trigger {
        soname_prefix: "last.so".into(),
    });
    error_contains(rule.validate(), "MAX_RECORDS");
    rule.triggers = vec![
        Trigger {
            soname_prefix: "duplicate.so".into()
        };
        MAX_RECORDS
    ];
    error_contains(rule.validate(), "MAX_RECORDS");
    let rules: Vec<_> = (0..MAX_RECORDS)
        .map(|i| descriptor(&format!("rule-{i}")))
        .collect();
    validate_catalog(&rules, &[], &[]).unwrap();
    error_contains(
        validate_catalog(&rules, &[descriptor("extra")], &[]),
        "MAX_RECORDS",
    );
    error_contains(
        validate_catalog(&rules, &[], &[id("rule-0")]),
        "MAX_RECORDS",
    );
    let mut nested = descriptor("nested");
    nested.runtime.paths = (0..(MAX_RECORDS - 1) / 3)
        .map(|i| runtime_path(&format!("path-{i}")))
        .collect();
    nested.validate().unwrap();
    nested.runtime.paths[0]
        .variables
        .push(EnvironmentName::new("ANOTHER_ALIAS").unwrap());
    error_contains(nested.validate(), "MAX_RECORDS");
}

#[test]
fn unsupported_programmatic_schema_is_rejected() {
    let mut rule = descriptor("test");
    for version in [0, SCHEMA_VERSION + 1, u16::MAX] {
        rule.schema = version;
        error_contains(rule.validate(), "unsupported version");
    }
}

#[cfg(feature = "rules-toml")]
mod toml_schema {
    use super::*;
    use onelf_format::rules_toml::parse_descriptor;

    const GST: &str = include_str!("../rules/gstreamer.toml");
    const ALSA: &str = include_str!("../rules/alsa.toml");
    const MAGIC: &str = include_str!("../rules/magic.toml");
    const HEIF: &str = include_str!("../rules/heif.toml");

    fn rejects_replacement(text: &str, from: &str, to: &str, expected: &str) {
        assert!(text.contains(from), "missing test pattern {from:?}");
        error_contains(parse_descriptor(&text.replacen(from, to, 1)), expected);
    }

    #[test]
    fn actual_framework_fixtures_preserve_order_aliases_policies_and_phases() {
        let gst = parse_descriptor(GST).unwrap();
        assert_eq!(gst.id.as_str(), "gstreamer");
        assert_eq!(gst.resources.plugins[0].requirement, Requirement::Optional);
        let scanner = &gst.resources.helpers[0];
        assert_eq!(scanner.requirement, Requirement::Required);
        assert_eq!(scanner.group.as_str(), "plugins");
        assert_eq!(scanner.candidates.len(), 3);
        assert_eq!(scanner.candidates[0].base, SourceBase::LibraryDir);
        assert_eq!(scanner.candidates[2].base, SourceBase::InstallationPrefix);
        let scanner = &gst.runtime.paths[1];
        assert_eq!(scanner.kind, ResourceKind::File);
        assert_eq!(scanner.selection, PathSelection::First);
        assert_eq!(
            scanner
                .variables
                .iter()
                .map(EnvironmentName::as_str)
                .collect::<Vec<_>>(),
            ["GST_PLUGIN_SCANNER_1_0", "GST_PLUGIN_SCANNER"]
        );
        assert_eq!(
            scanner.candidates[1].path.as_str(),
            "gstreamer1.0/gstreamer-1.0/gst-plugin-scanner"
        );
        assert_eq!(scanner.candidates[2].base, RuntimeBase::PackageRoot);
        assert_eq!(gst.runtime.paths[0].selection, PathSelection::All);
        assert_eq!(gst.runtime.paths[0].inheritance, Inheritance::Replace);
        let alsa = parse_descriptor(ALSA).unwrap();
        assert_eq!(alsa.build.hooks, [BuildHookId::AlsaConfPatch]);
        assert_eq!(
            alsa.runtime.hooks,
            [RuntimeHookId::AlsaConfigPathNaturalSort]
        );
        let magic = parse_descriptor(MAGIC).unwrap();
        assert_eq!(magic.resources.data[0].kind, ResourceKind::File);
        assert_eq!(magic.resources.data[0].candidates.len(), 2);
        assert_eq!(magic.runtime.paths[0].inheritance, Inheritance::Preserve);
        let heif = parse_descriptor(HEIF).unwrap();
        assert_eq!(
            heif.runtime.paths[0].selection,
            PathSelection::FirstPerLibraryRoot
        );
        assert_eq!(
            heif.runtime.paths[0].candidates[0].path.as_str(),
            "libheif/plugins"
        );
        validate_catalog(&[gst, alsa, magic, heif], &[], &[]).unwrap();
    }

    #[test]
    fn omitted_collections_are_empty_in_absent_and_present_tables() {
        let minimal = "schema = 1\nid = 'minimal'\n";
        let expected = descriptor("minimal");
        assert_eq!(parse_descriptor(minimal).unwrap(), expected);
        for tables in [
            "[resources]\n[build]\n[runtime]\n",
            "[runtime]\nhooks = []\n[resources]\nplugins = []\n",
            "[build]\nhooks = []\n[runtime]\npaths = []\n",
        ] {
            assert_eq!(
                parse_descriptor(&format!("{minimal}{tables}")).unwrap(),
                expected
            );
        }
        assert!(parse_descriptor(GST).unwrap().resources.data.is_empty());
        assert!(parse_descriptor(MAGIC).unwrap().runtime.hooks.is_empty());
    }

    #[test]
    fn required_fields_do_not_silently_default() {
        for text in [
            "",
            "schema = 1",
            "id = 'test'",
            "schema = 1\nid = 'test'\n[[triggers]]",
        ] {
            error_contains(parse_descriptor(text), "missing field");
        }
        rejects_replacement(MAGIC, "kind = \"file\"\n", "", "missing field `kind`");
        rejects_replacement(
            MAGIC,
            "requirement = \"required\"\n",
            "",
            "missing field `requirement`",
        );
        rejects_replacement(
            MAGIC,
            "selection = \"first\"\n",
            "",
            "missing field `selection`",
        );
        rejects_replacement(
            MAGIC,
            "inheritance = \"preserve\"\n",
            "",
            "missing field `inheritance`",
        );
        rejects_replacement(
            MAGIC,
            "variables = [\"MAGIC\"]",
            "variables = []",
            "environment alias",
        );
        rejects_replacement(
            GST,
            "candidates = [{ base = \"library-dir\", path = \"gstreamer-1.0\" }]",
            "",
            "at least one source or runtime candidate",
        );
    }

    #[test]
    fn wrong_field_types_are_not_treated_as_omitted_collections() {
        for (text, expected) in [
            ("schema = true\nid = 'test'", "integer version"),
            ("schema = -1\nid = 'test'", "unsigned 16-bit version"),
            ("schema = 65536\nid = 'test'", "unsigned 16-bit version"),
            ("schema = 1\nid = 5", "expected a string"),
            (
                "schema = 1\nid = 'test'\ntriggers = {}",
                "expected an array",
            ),
            (
                "schema = 1\nid = 'test'\ntriggers = [1]",
                "expected a table",
            ),
            (
                "schema = 1\nid = 'test'\nresources = []",
                "expected a table",
            ),
            (
                "schema = 1\nid = 'test'\n[build]\nhooks = true",
                "expected an array",
            ),
            (
                "schema = 1\nid = 'test'\n[runtime]\nhooks = [1]",
                "expected a string",
            ),
        ] {
            error_contains(parse_descriptor(text), expected);
        }
    }

    #[test]
    fn every_nested_table_rejects_unknown_fields_and_underscored_spelling() {
        for text in [
            "schema = 1\nid = 'test'\nunknown = true",
            "schema = 1\nid = 'test'\n[resources]\nunknown = []",
            "schema = 1\nid = 'test'\n[build]\nunknown = []",
            "schema = 1\nid = 'test'\n[runtime]\nunknown = []",
        ] {
            error_contains(parse_descriptor(text), "unknown field");
        }
        for (text, from, to) in [
            (GST, "soname-prefix =", "soname_prefix ="),
            (
                GST,
                "[[resources.plugins]]",
                "[[resources.plugins]]\nunknown = true",
            ),
            (
                MAGIC,
                "[[resources.data]]",
                "[[resources.data]]\nunknown = true",
            ),
            (
                GST,
                "[[resources.helpers]]",
                "[[resources.helpers]]\nunknown = true",
            ),
            (
                GST,
                "[[runtime.paths]]",
                "[[runtime.paths]]\nunknown = true",
            ),
            (
                GST,
                "base = \"library-dir\"",
                "unknown = true, base = \"library-dir\"",
            ),
            (
                GST,
                "base = \"library-root\"",
                "unknown = true, base = \"library-root\"",
            ),
            (ALSA, "[build]", "[build]\nstage = 'arbitrary'"),
            (ALSA, "[runtime]", "[runtime]\ncommand = '/bin/sh'"),
        ] {
            rejects_replacement(text, from, to, "unknown field");
        }
    }

    #[test]
    fn unknown_versions_enums_and_cross_phase_hooks_fail() {
        for version in ["0", "2", "65535"] {
            rejects_replacement(
                GST,
                "schema = 1",
                &format!("schema = {version}"),
                "unsupported version",
            );
        }
        for (text, from, to) in [
            (GST, "kind = \"directory\"", "kind = \"socket\""),
            (GST, "requirement = \"optional\"", "requirement = \"maybe\""),
            (GST, "base = \"library-dir\"", "base = \"host-root\""),
            (GST, "base = \"library-root\"", "base = \"host-root\""),
            (GST, "selection = \"all\"", "selection = \"glob\""),
            (
                GST,
                "inheritance = \"replace\"",
                "inheritance = \"append-host\"",
            ),
            (ALSA, "alsa_conf_patch", "alsa_config_path_natural_sort"),
            (ALSA, "alsa_config_path_natural_sort", "alsa_conf_patch"),
            (ALSA, "alsa_conf_patch", "/bin/sh"),
            (ALSA, "alsa_config_path_natural_sort", "unknown_hook"),
        ] {
            rejects_replacement(text, from, to, "unknown variant");
        }
        rejects_replacement(
            GST,
            "kind = \"directory\"",
            "kind = \"file\"",
            "plugins.kind",
        );
        rejects_replacement(
            GST,
            "group = \"plugins\"\nkind = \"file\"",
            "group = \"plugins\"\nkind = \"directory\"",
            "helpers.kind",
        );
        rejects_replacement(
            HEIF,
            "base = \"library-root\"",
            "base = \"package-root\"",
            "requires library-root",
        );
        let hooks = "schema = 1\nid = 'hooks'\n[runtime]\nhooks = ['gdk_cache_absolutise', 'fontconfig_generate', 'gio_module_paths', 'tcl_tk_libraries']";
        assert_eq!(
            parse_descriptor(hooks).unwrap().runtime.hooks,
            [
                RuntimeHookId::GdkCacheAbsolutise,
                RuntimeHookId::FontconfigGenerate,
                RuntimeHookId::GioModulePaths,
                RuntimeHookId::TclTkLibraries
            ]
        );
    }

    #[test]
    fn all_path_positions_reject_unsafe_values_and_normalize_relative_dots() {
        for value in [
            "",
            ".",
            "../escape",
            "lib/../../escape",
            "/etc/passwd",
            "C:/Windows",
            "C:Windows",
            "./C:/Windows",
            "a\\b",
            "\\\\host\\share",
        ] {
            for (text, from, field) in [
                (GST, "destination = \"lib/gstreamer-1.0\"", "destination"),
                (GST, "path = \"gstreamer-1.0\"", "path"),
                (
                    GST,
                    "path = \"gstreamer1.0/gstreamer-1.0/gst-plugin-scanner\"",
                    "path",
                ),
                (MAGIC, "path = \"share/file/misc/magic.mgc\"", "path"),
            ] {
                rejects_replacement(text, from, &format!("{field} = '{value}'"), "path:");
            }
        }
        let rule =
            parse_descriptor(&GST.replace("\"lib/gstreamer-1.0\"", "\"./lib//gstreamer-1.0/.\""))
                .unwrap();
        assert_eq!(
            rule.resources.plugins[0].destination.as_str(),
            "lib/gstreamer-1.0"
        );
        rejects_replacement(
            GST,
            "path = \"gstreamer-1.0\"",
            &format!("path = '{}'", "a".repeat(MAX_PATH_BYTES + 1)),
            "MAX_PATH_BYTES",
        );
        let runtime = GST.replacen("path = \"gstreamer-1.0\"", "path = \"safe\"", 1);
        rejects_replacement(
            &runtime,
            "path = \"gstreamer-1.0\"",
            "path = '../escape'",
            "path:",
        );
    }

    #[test]
    fn plugin_members_are_validated_paths_and_data_groups_take_none() {
        let listed = "destination = \"lib/gstreamer-1.0\"\nmembers = [\"core/libcore.so\", \"./libother.so\"]";
        let rule =
            parse_descriptor(&GST.replacen("destination = \"lib/gstreamer-1.0\"", listed, 1))
                .unwrap();
        let members: Vec<&str> = rule.resources.plugins[0]
            .members
            .iter()
            .map(RelativePath::as_str)
            .collect();
        assert_eq!(members, ["core/libcore.so", "libother.so"]);
        assert!(
            parse_descriptor(GST).unwrap().resources.plugins[0]
                .members
                .is_empty()
        );
        let destination = "destination = \"lib/gstreamer-1.0\"";
        for value in [
            "\"../escape.so\"",
            "\"/usr/lib/escape.so\"",
            "\"a\\u0000b.so\"",
            "\"\"",
        ] {
            rejects_replacement(
                GST,
                destination,
                &format!("{destination}\nmembers = [{value}]"),
                "path",
            );
        }
        rejects_replacement(
            GST,
            destination,
            &format!("{destination}\nmembers = []"),
            "at least one member",
        );
        rejects_replacement(
            GST,
            destination,
            &format!("{destination}\nmembers = [\"a.so\", \"./a.so\"]"),
            "duplicate",
        );
        rejects_replacement(
            MAGIC,
            "[[resources.data]]",
            "[[resources.data]]\nmembers = [\"magic.mgc\"]",
            "unknown field",
        );
        let mut direct = parse_descriptor(MAGIC).unwrap();
        direct.resources.data[0].members = vec![path("magic.mgc")];
        error_contains(direct.validate(), "only plugin groups list members");
    }

    #[test]
    fn invalid_names_and_library_search_variables_cannot_enter_through_toml() {
        for value in [
            "",
            "1BAD",
            "A-B",
            "A=B",
            "LD_LIBRARY_PATH",
            "LD_LIBRARY_PATH_32",
            "LD_LIBRARY_PATH_64",
            "LD_AOUT_LIBRARY_PATH",
            "LD_PRELOAD",
            "LD_AUDIT",
            "GCONV_PATH",
            "ONELF_DIR",
        ] {
            rejects_replacement(
                MAGIC,
                "variables = [\"MAGIC\"]",
                &format!("variables = ['{value}']"),
                "environment:",
            );
        }
        rejects_replacement(GST, "id = \"gstreamer\"", "id = ''", "id:");
        rejects_replacement(
            GST,
            "id = \"gstreamer\"",
            &format!("id = '{}'", "a".repeat(MAX_ID_BYTES + 1)),
            "MAX_ID_BYTES",
        );
        rejects_replacement(
            GST,
            "soname-prefix = \"libgstreamer-1.0.so\"",
            "soname-prefix = 'lib*.so'",
            "soname-prefix",
        );
    }

    #[test]
    fn duplicate_parsed_identities_and_associations_are_rejected() {
        rejects_replacement(GST, "id = \"scanner\"", "id = \"plugins\"", "resource id");
        rejects_replacement(GST, "group = \"plugins\"", "group = \"missing\"", "missing");
        rejects_replacement(
            GST,
            "variables = [\"GST_PLUGIN_SCANNER_1_0\", \"GST_PLUGIN_SCANNER\"]",
            "variables = ['GST_PLUGIN_SCANNER', 'GST_PLUGIN_SCANNER']",
            "environment alias",
        );
        let rule = parse_descriptor(GST).unwrap();
        error_contains(
            validate_catalog(&[], &[rule.clone(), rule], &[]),
            "gstreamer",
        );
    }

    #[test]
    fn text_and_record_limits_accept_the_boundary_and_reject_overflow() {
        let mut text = "schema = 1\nid = 'boundary'\n#".to_owned();
        text.push_str(&"x".repeat(MAX_DESCRIPTOR_BYTES - text.len()));
        parse_descriptor(&text).unwrap();
        text.push('x');
        error_contains(parse_descriptor(&text), "MAX_DESCRIPTOR_BYTES");
        let mut text = "schema = 1\nid = 'records'\n".to_owned();
        for i in 0..MAX_RECORDS - 1 {
            text.push_str(&format!("[[triggers]]\nsoname-prefix = 'lib{i}.so'\n"));
        }
        parse_descriptor(&text).unwrap();
        text.push_str("[[triggers]]\nsoname-prefix = 'one-too-many.so'\n");
        error_contains(parse_descriptor(&text), "MAX_RECORDS");
    }

    #[test]
    fn malformed_truncated_and_deep_inputs_fail_without_partial_results() {
        for text in [
            "schema = 1\nid = 'test",
            "schema = 1\nid = 'test'\n[runtime",
            "schema = 1\nid = 'test'\ntriggers = [{ soname-prefix = 'lib.so'",
            "schema = 1\nid = 'test'\ntriggers = [",
        ] {
            error_contains(parse_descriptor(text), "TOML line");
        }
        let text = format!(
            "schema = 1\nid = 'test'\nunknown = {}0{}",
            "[".repeat(256),
            "]".repeat(256)
        );
        assert!(parse_descriptor(&text).is_err());
        let text = format!(
            "schema = 1\nid = 'test'\n[{}]\nvalue = 1",
            std::iter::repeat_n("deep", 256)
                .collect::<Vec<_>>()
                .join(".")
        );
        assert!(parse_descriptor(&text).is_err());
        let text = format!("schema = 1\nid = 'test'\n{} = true", "x".repeat(100_000));
        assert!(parse_descriptor(&text).unwrap_err().to_string().len() < 4096);
    }
}

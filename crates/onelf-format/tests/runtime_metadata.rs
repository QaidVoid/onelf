use onelf_format::rules::{
    EnvironmentName, Inheritance, MAX_ID_BYTES, MAX_PATH_BYTES, MAX_RECORDS, PathSelection,
    RelativePath, ResourceKind, RuleId, RuntimeBase, RuntimeCandidate, RuntimeHookId,
    RuntimePathGroup, RuntimeRules,
};
use onelf_format::runtime_metadata::{
    EntrypointSelection, EntrypointSets, HEADER_BYTES, MAX_METADATA_BYTES, RuntimeRuleMetadata,
    RuntimeRuleRecord, SetPin,
};

fn id(value: &str) -> RuleId {
    RuleId::new(value).unwrap()
}

fn pin(name: &str, byte: u8) -> SetPin {
    SetPin {
        name: id(name),
        hash: [byte; 32],
    }
}

fn path() -> RuntimePathGroup {
    RuntimePathGroup {
        id: id("plugins"),
        kind: ResourceKind::Directory,
        candidates: vec![RuntimeCandidate {
            base: RuntimeBase::LibraryRoot,
            path: RelativePath::new("libheif/plugins").unwrap(),
        }],
        variables: vec![EnvironmentName::new("PLUGIN_PATH").unwrap()],
        selection: PathSelection::All,
        inheritance: Inheritance::Replace,
    }
}

fn rules() -> RuntimeRuleMetadata {
    RuntimeRuleMetadata {
        rules: vec![RuntimeRuleRecord {
            id: id("framework"),
            binding: None,
            runtime: RuntimeRules {
                paths: vec![path()],
                hooks: vec![RuntimeHookId::GioModulePaths],
            },
        }],
    }
}

fn selections() -> EntrypointSets {
    EntrypointSets {
        pins: vec![pin("s", 0x11)],
        entrypoints: vec![EntrypointSelection {
            name: "app".into(),
            sets: vec![id("s")],
        }],
    }
}

fn hex(value: &str) -> Vec<u8> {
    value
        .split_whitespace()
        .map(|part| u8::from_str_radix(part, 16).unwrap())
        .collect()
}

fn put_u32(bytes: &mut [u8], offset: usize, value: usize) {
    bytes[offset..offset + 4].copy_from_slice(&u32::try_from(value).unwrap().to_le_bytes());
}

fn get_u32(bytes: &[u8], offset: usize) -> usize {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize
}

fn repair_payload(bytes: &mut [u8]) {
    put_u32(bytes, 8, bytes.len() - HEADER_BYTES);
}

#[test]
fn fixed_rule_fixture_and_empty_tables_are_byte_exact() {
    let value = RuntimeRuleMetadata {
        rules: vec![RuntimeRuleRecord {
            id: id("x"),
            binding: None,
            runtime: RuntimeRules::default(),
        }],
    };
    let fixture = hex("4f 52 55 4c 01 00 00 00 17 00 00 00 01 00 00 00
                       01 00 00 00 0f 00 00 00 01 01 00 00 00 78 00
                       00 00 00 00 00 00 00 00");
    assert_eq!(value.encode().unwrap(), fixture);
    assert_eq!(RuntimeRuleMetadata::decode(&fixture).unwrap(), value);
    let empty_rules = hex("4f 52 55 4c 01 00 00 00 04 00 00 00 00 00 00 00 00 00 00 00");
    let empty_sets = hex("4f 53 45 54 01 00 00 00 08 00 00 00 00 00 00 00
                          00 00 00 00 00 00 00 00");
    assert_eq!(
        RuntimeRuleMetadata::default().encode().unwrap(),
        empty_rules
    );
    assert_eq!(EntrypointSets::default().encode().unwrap(), empty_sets);
    assert_eq!(
        RuntimeRuleMetadata::decode(&empty_rules)
            .unwrap()
            .encode()
            .unwrap(),
        empty_rules
    );
    assert_eq!(
        EntrypointSets::decode(&empty_sets)
            .unwrap()
            .encode()
            .unwrap(),
        empty_sets
    );
}

#[test]
fn fixed_selection_fixture_uses_full_hash_and_named_references() {
    let fixture = hex("4f 53 45 54 01 00 00 00 47 00 00 00 03 00 00 00
                       01 00 00 00 26 00 00 00 01 01 00 00 00 73
                       11 11 11 11 11 11 11 11 11 11 11 11 11 11 11 11
                       11 11 11 11 11 11 11 11 11 11 11 11 11 11 11 11
                       01 00 00 00 11 00 00 00 02 03 00 00 00 61 70 70
                       01 00 00 00 01 00 00 00 73");
    assert_eq!(selections().encode().unwrap(), fixture);
    assert_eq!(EntrypointSets::decode(&fixture).unwrap(), selections());
}

#[test]
fn all_runtime_fields_and_variant_tags_round_trip() {
    let mut value = rules();
    value.rules[0].binding = Some(pin("toolkit", 0xa5));
    value.rules[0].runtime.paths.clear();
    for (index, (kind, selection, inheritance, base)) in [
        (
            ResourceKind::File,
            PathSelection::First,
            Inheritance::Preserve,
            RuntimeBase::PackageRoot,
        ),
        (
            ResourceKind::Directory,
            PathSelection::All,
            Inheritance::Replace,
            RuntimeBase::LibraryRoot,
        ),
        (
            ResourceKind::Directory,
            PathSelection::FirstPerLibraryRoot,
            Inheritance::Preserve,
            RuntimeBase::LibraryRoot,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let mut group = path();
        group.id = id(&format!("group{index}"));
        group.kind = kind;
        group.selection = selection;
        group.inheritance = inheritance;
        group.candidates[0].base = base;
        group.candidates.push(RuntimeCandidate {
            base,
            path: RelativePath::new("alternative").unwrap(),
        });
        group.variables.push(EnvironmentName::new("ALIAS").unwrap());
        value.rules[0].runtime.paths.push(group);
    }
    value.rules[0].runtime.hooks = vec![
        RuntimeHookId::AlsaConfigPathNaturalSort,
        RuntimeHookId::GdkCacheAbsolutise,
        RuntimeHookId::FontconfigGenerate,
        RuntimeHookId::GioModulePaths,
        RuntimeHookId::TclTkLibraries,
    ];
    let bytes = value.encode().unwrap();
    assert_eq!(RuntimeRuleMetadata::decode(&bytes).unwrap(), value);
    assert_eq!(
        RuntimeRuleMetadata::decode(&bytes)
            .unwrap()
            .encode()
            .unwrap(),
        bytes
    );
}

#[test]
fn selection_supports_65_sets_and_unicode_manifest_names() {
    let pins: Vec<_> = (0..65)
        .rev()
        .map(|index| pin(&format!("set{index:03}"), index))
        .collect();
    let value = EntrypointSets {
        entrypoints: vec![
            EntrypointSelection {
                name: "\u{03bb}-gui".into(),
                sets: pins.iter().map(|pin| pin.name.clone()).collect(),
            },
            EntrypointSelection {
                name: "cli".into(),
                sets: vec![],
            },
        ],
        pins,
    };
    let bytes = value.encode().unwrap();
    let decoded = EntrypointSets::decode(&bytes).unwrap();
    assert_eq!(decoded.entrypoints[1].sets.len(), 65);
    assert!(decoded.entrypoints[0].sets.is_empty());
    decoded
        .validate_against(&["cli", "\u{03bb}-gui"], &value.pins)
        .unwrap();
    assert_eq!(decoded.encode().unwrap(), bytes);
    let mut reordered = value.clone();
    reordered.pins.reverse();
    reordered.entrypoints.reverse();
    reordered.entrypoints[0].sets.reverse();
    reordered.entrypoints[1].sets.reverse();
    assert_eq!(reordered.encode().unwrap(), bytes);
}

#[test]
fn canonical_scoped_identity_preserves_runtime_declaration_order() {
    let mut value = rules();
    let mut bound = value.rules[0].clone();
    bound.binding = Some(pin("gui", 1));
    value.rules.insert(0, bound.clone());
    let mut other_pin = bound;
    other_pin.binding = Some(pin("other", 2));
    value.rules.insert(0, other_pin);
    let bytes = value.encode().unwrap();
    value.rules.reverse();
    assert_eq!(value.encode().unwrap(), bytes);
    let decoded = RuntimeRuleMetadata::decode(&bytes).unwrap();
    assert_eq!(decoded.rules.len(), 3);
    assert!(decoded.rules[0].binding.is_none());
    let mut changed_order = decoded.clone();
    changed_order.rules[0].runtime.paths[0]
        .candidates
        .push(RuntimeCandidate {
            base: RuntimeBase::PackageRoot,
            path: RelativePath::new("alternate").unwrap(),
        });
    let before = changed_order.encode().unwrap();
    changed_order.rules[0].runtime.paths[0].candidates.reverse();
    assert_ne!(changed_order.encode().unwrap(), before);
}

#[test]
fn duplicate_identities_aliases_hooks_and_empty_groups_are_errors() {
    let mut value = rules();
    value.rules.push(value.rules[0].clone());
    assert!(
        value
            .encode()
            .unwrap_err()
            .to_string()
            .contains("duplicate")
    );
    let mut value = rules();
    value.rules[0].runtime.paths.push(path());
    assert!(value.encode().is_err());
    let mut value = rules();
    value.rules[0].runtime.paths[0]
        .variables
        .push(EnvironmentName::new("PLUGIN_PATH").unwrap());
    assert!(value.encode().is_err());
    let mut value = rules();
    value.rules[0]
        .runtime
        .hooks
        .push(RuntimeHookId::GioModulePaths);
    assert!(value.encode().is_err());
    let mut value = rules();
    value.rules[0].runtime.paths[0].candidates.clear();
    assert!(value.encode().is_err());
    let mut value = rules();
    value.rules[0].runtime.paths[0].variables.clear();
    assert!(value.encode().is_err());
    let mut value = rules();
    value.rules[0].runtime.paths[0].selection = PathSelection::FirstPerLibraryRoot;
    value.rules[0].runtime.paths[0].candidates[0].base = RuntimeBase::PackageRoot;
    assert!(value.encode().is_err());
}

#[test]
fn selection_duplicates_unknown_names_and_pin_conflicts_are_errors() {
    let mut value = selections();
    value.pins.push(pin("s", 0x22));
    assert!(value.encode().is_err());
    let mut value = selections();
    value.pins.push(value.pins[0].clone());
    assert!(value.encode().is_err());
    let mut value = selections();
    value.entrypoints.push(value.entrypoints[0].clone());
    assert!(value.encode().is_err());
    let mut value = selections();
    value.entrypoints[0].sets.push(id("s"));
    assert!(value.encode().is_err());
    let mut value = selections();
    value.entrypoints[0].sets = vec![id("unknown")];
    assert!(value.encode().is_err());
    let mut same_artifact = selections();
    same_artifact.pins.push(pin("other-name", 0x11));
    assert!(same_artifact.encode().is_ok());
}

#[test]
fn final_manifest_pin_tables_and_prepared_bindings_must_match() {
    let value = selections();
    value.validate_against(&["app"], &value.pins).unwrap();
    for names in [
        vec![],
        vec!["renamed"],
        vec!["app", "other"],
        vec!["app", "app"],
    ] {
        assert!(value.validate_against(&names, &value.pins).is_err());
    }
    assert!(value.validate_against(&["app"], &[]).is_err());
    assert!(value.validate_against(&["app"], &[pin("s", 0x22)]).is_err());
    assert!(
        value
            .validate_against(&["app"], &[pin("other", 0x11)])
            .is_err()
    );
    assert!(
        value
            .validate_against(&["app"], &[pin("s", 0x11), pin("s", 0x11)])
            .is_err()
    );
    assert!(
        EntrypointSets::default()
            .validate_against(&["app"], &[])
            .is_err()
    );
    let mut runtime = rules();
    runtime.validate_bindings(&value).unwrap();
    runtime.rules[0].binding = Some(pin("s", 0x11));
    runtime.validate_bindings(&value).unwrap();
    runtime.rules[0].binding = Some(pin("s", 0x22));
    assert!(runtime.validate_bindings(&value).is_err());
    runtime.rules[0].binding = Some(pin("unknown", 0x11));
    assert!(runtime.validate_bindings(&value).is_err());
}

#[test]
fn every_truncated_offset_is_rejected_without_panics() {
    let runtime = rules().encode().unwrap();
    for end in 0..runtime.len() {
        assert!(
            RuntimeRuleMetadata::decode(&runtime[..end]).is_err(),
            "offset {end}"
        );
    }
    let sets = selections().encode().unwrap();
    for end in 0..sets.len() {
        assert!(
            EntrypointSets::decode(&sets[..end]).is_err(),
            "offset {end}"
        );
    }
}

#[test]
fn header_bitflips_versions_flags_sizes_and_record_counts_are_rejected() {
    for original in [rules().encode().unwrap(), selections().encode().unwrap()] {
        let decode = |bytes: &[u8]| {
            if original.starts_with(b"ORUL") {
                RuntimeRuleMetadata::decode(bytes).map(|_| ())
            } else {
                EntrypointSets::decode(bytes).map(|_| ())
            }
        };
        for offset in 0..HEADER_BYTES {
            for bit in 0..8 {
                let mut bytes = original.clone();
                bytes[offset] ^= 1 << bit;
                assert!(decode(&bytes).is_err(), "offset {offset}, bit {bit}");
            }
        }
        for offset in [8, 12, 16] {
            let mut bytes = original.clone();
            put_u32(&mut bytes, offset, u32::MAX as usize);
            assert!(decode(&bytes).is_err());
        }
        let mut trailing = original.clone();
        trailing.push(0);
        assert!(decode(&trailing).is_err());
        repair_payload(&mut trailing);
        assert!(decode(&trailing).is_err());
    }
}

#[test]
fn record_and_nested_length_allocation_bombs_are_errors() {
    let original = rules().encode().unwrap();
    let rule_body = 25;
    let paths_count = rule_body + 4 + "framework".len() + 1;
    let path_record = paths_count + 4;
    let path_body = path_record + 5;
    let candidates_count = path_body + 4 + "plugins".len() + 3;
    let candidate_record = candidates_count + 4;
    let candidate_string = candidate_record + 6;
    let variables_count = candidate_record + 4 + get_u32(&original, candidate_record);
    let alias_string = variables_count + 4;
    let hooks_count = path_record + 4 + get_u32(&original, path_record);
    for offset in [
        20,
        rule_body,
        paths_count,
        path_record,
        path_body,
        candidates_count,
        candidate_record,
        candidate_string,
        variables_count,
        alias_string,
        hooks_count,
    ] {
        let mut bytes = original.clone();
        put_u32(&mut bytes, offset, u32::MAX as usize);
        assert!(
            RuntimeRuleMetadata::decode(&bytes).is_err(),
            "offset {offset}"
        );
    }
    let original = selections().encode().unwrap();
    let entry_count = 20 + 4 + get_u32(&original, 20);
    for offset in [
        20,
        25,
        entry_count,
        entry_count + 4,
        entry_count + 9,
        entry_count + 16,
        entry_count + 20,
    ] {
        let mut bytes = original.clone();
        put_u32(&mut bytes, offset, u32::MAX as usize);
        assert!(EntrypointSets::decode(&bytes).is_err(), "offset {offset}");
    }
}

#[test]
fn unknown_typed_variants_and_binding_flags_are_fatal() {
    let mut value = rules();
    value.rules[0].binding = Some(pin("s", 1));
    let original = value.encode().unwrap();
    let binding_flag = 25 + 4 + "framework".len();
    let binding_record = binding_flag + 1;
    let paths_count = binding_record + 4 + get_u32(&original, binding_record);
    let path_record = paths_count + 4;
    let kind = path_record + 5 + 4 + "plugins".len();
    let candidate_record = kind + 3 + 4;
    let hooks_count = path_record + 4 + get_u32(&original, path_record);
    let hook_record = hooks_count + 4;
    for offset in [
        24,
        binding_flag,
        binding_record + 4,
        path_record + 4,
        kind,
        kind + 1,
        kind + 2,
        candidate_record + 4,
        candidate_record + 5,
        hook_record + 4,
        hook_record + 5,
    ] {
        let mut bytes = original.clone();
        bytes[offset] = 255;
        assert!(
            RuntimeRuleMetadata::decode(&bytes).is_err(),
            "offset {offset}"
        );
    }
    let original = selections().encode().unwrap();
    let entry_count = 20 + 4 + get_u32(&original, 20);
    for offset in [24, entry_count + 8] {
        let mut bytes = original.clone();
        bytes[offset] = 255;
        assert!(EntrypointSets::decode(&bytes).is_err());
    }
}

fn replace_equal_length(bytes: &mut [u8], from: &[u8], to: &[u8]) {
    assert_eq!(from.len(), to.len());
    let offset = bytes
        .windows(from.len())
        .position(|slice| slice == from)
        .unwrap();
    bytes[offset..offset + to.len()].copy_from_slice(to);
}

#[test]
fn invalid_names_environment_paths_and_utf8_are_rejected_on_decode() {
    let original = rules().encode().unwrap();
    for replacement in [
        b"../heif/plugins".as_slice(),
        b"/ibheif/plugins",
        b"libheif\0plugins",
        b"libheif\nplugins",
        b"libheif\\plugins",
    ] {
        let mut bytes = original.clone();
        replace_equal_length(&mut bytes, b"libheif/plugins", replacement);
        assert!(RuntimeRuleMetadata::decode(&bytes).is_err());
    }
    let mut bytes = original.clone();
    replace_equal_length(&mut bytes, b"framework", b"bad/name!");
    assert!(RuntimeRuleMetadata::decode(&bytes).is_err());
    let mut bytes = original.clone();
    replace_equal_length(&mut bytes, b"PLUGIN_PATH", b"BAD=ENV_VAR");
    assert!(RuntimeRuleMetadata::decode(&bytes).is_err());
    let mut value = rules();
    value.rules[0].runtime.paths[0].variables[0] = EnvironmentName::new("SAFE_PLUGINPATH").unwrap();
    let mut bytes = value.encode().unwrap();
    replace_equal_length(&mut bytes, b"SAFE_PLUGINPATH", b"LD_LIBRARY_PATH");
    assert!(
        RuntimeRuleMetadata::decode(&bytes)
            .unwrap_err()
            .to_string()
            .contains("prohibited")
    );
    for (replacement, reason) in [
        (b"LD_PRELOAD_PATH", "loader-control"),
        (b"ONELF_PLUGINPAT", "runtime-owned"),
        (b"FONTCONFIG_FILE", "runtime hook or launch procedure"),
    ] {
        let mut bytes = value.encode().unwrap();
        replace_equal_length(&mut bytes, b"SAFE_PLUGINPATH", replacement);
        let error = RuntimeRuleMetadata::decode(&bytes).unwrap_err().to_string();
        assert!(error.contains(reason), "{error}");
    }
    let mut dotted = selections();
    dotted.pins[0].name = id("s.t");
    dotted.entrypoints[0].sets = vec![id("s.t")];
    assert!(dotted.encode().is_err(), "a dotted set name is not a pin");
    let mut bytes = selections().encode().unwrap();
    replace_equal_length(&mut bytes, b"app", &[0xff, b'p', b'p']);
    assert!(EntrypointSets::decode(&bytes).is_err());
    let mut bytes = selections().encode().unwrap();
    replace_equal_length(&mut bytes, b"app", b"a\0p");
    assert!(EntrypointSets::decode(&bytes).is_err());
}

#[test]
fn normalized_shared_paths_are_encoded_and_nonnormalized_wire_paths_are_rejected() {
    let mut value = rules();
    value.rules[0].runtime.paths[0].candidates[0].path =
        RelativePath::new("lib//./plugins/").unwrap();
    assert_eq!(
        value.rules[0].runtime.paths[0].candidates[0].path.as_str(),
        "lib/plugins"
    );
    let original = value.encode().unwrap();
    let mut bytes = original.clone();
    replace_equal_length(&mut bytes, b"lib/plugins", b"lib//plugin");
    assert!(
        RuntimeRuleMetadata::decode(&bytes)
            .unwrap_err()
            .to_string()
            .contains("nonnormalized")
    );
    assert_eq!(RuntimeRuleMetadata::decode(&original).unwrap(), value);
}

#[test]
fn identifier_entrypoint_environment_and_path_boundaries() {
    let longest = "a".repeat(MAX_ID_BYTES);
    let mut value = rules();
    value.rules[0].id = id(&longest);
    value.rules[0].runtime.paths[0].id = id(&longest);
    value.rules[0].runtime.paths[0].variables[0] = EnvironmentName::new(&longest).unwrap();
    value.rules[0].runtime.paths[0].candidates[0].path =
        RelativePath::new(&"p".repeat(MAX_PATH_BYTES)).unwrap();
    let bytes = value.encode().unwrap();
    assert_eq!(RuntimeRuleMetadata::decode(&bytes).unwrap(), value);
    assert!(RuleId::new(&"a".repeat(MAX_ID_BYTES + 1)).is_err());
    assert!(EnvironmentName::new(&"A".repeat(MAX_ID_BYTES + 1)).is_err());
    assert!(RelativePath::new(&"p".repeat(MAX_PATH_BYTES + 1)).is_err());
    let mut sets = selections();
    sets.entrypoints[0].name = "x".repeat(MAX_ID_BYTES);
    assert!(sets.encode().is_ok());
    sets.entrypoints[0].name.push('x');
    assert!(sets.encode().is_err());
    sets.entrypoints[0].name.clear();
    assert!(sets.encode().is_err());
    sets.entrypoints[0].name = "bad\0name".into();
    assert!(sets.encode().is_err());
    let original = rules().encode().unwrap();
    for (offset, too_large) in [(25, MAX_ID_BYTES + 1), (70, MAX_PATH_BYTES + 1)] {
        let mut bytes = original.clone();
        put_u32(&mut bytes, offset, too_large);
        assert!(RuntimeRuleMetadata::decode(&bytes).is_err());
    }
}

#[test]
fn aggregate_record_bound_includes_nested_references_and_aliases() {
    let mut sets = EntrypointSets {
        pins: vec![pin("s", 1)],
        entrypoints: (0..MAX_RECORDS - 1)
            .map(|index| EntrypointSelection {
                name: format!("entry{index:04}"),
                sets: vec![],
            })
            .collect(),
    };
    let bytes = sets.encode().unwrap();
    assert_eq!(get_u32(&bytes, 12), MAX_RECORDS);
    assert_eq!(
        EntrypointSets::decode(&bytes).unwrap().entrypoints.len(),
        MAX_RECORDS - 1
    );
    sets.entrypoints[0].sets.push(id("s"));
    assert!(
        sets.encode()
            .unwrap_err()
            .to_string()
            .contains("MAX_RECORDS")
    );
    let mut value = rules();
    value.rules[0].runtime.hooks.clear();
    value.rules[0].runtime.paths[0].variables = (0..MAX_RECORDS - 3)
        .map(|index| EnvironmentName::new(&format!("VAR_{index}")).unwrap())
        .collect();
    let bytes = value.encode().unwrap();
    assert_eq!(get_u32(&bytes, 12), MAX_RECORDS);
    assert!(RuntimeRuleMetadata::decode(&bytes).is_ok());
    value.rules[0].runtime.paths[0]
        .variables
        .push(EnvironmentName::new("TOO_MANY").unwrap());
    assert!(value.encode().is_err());
}

#[test]
fn exact_one_mib_boundary_and_larger_files() {
    let mut value = rules();
    value.rules[0].runtime.hooks.clear();
    value.rules[0].runtime.paths[0].candidates.clear();
    let candidate = RuntimeCandidate {
        base: RuntimeBase::LibraryRoot,
        path: RelativePath::new(&"p".repeat(MAX_PATH_BYTES)).unwrap(),
    };
    value.rules[0].runtime.paths[0].candidates = vec![candidate; 255];
    let base_size = value.encode().unwrap().len();
    assert!(base_size < MAX_METADATA_BYTES);
    let final_path_bytes = MAX_METADATA_BYTES - base_size - 10;
    assert!(final_path_bytes <= MAX_PATH_BYTES && final_path_bytes > 0);
    value.rules[0].runtime.paths[0]
        .candidates
        .push(RuntimeCandidate {
            base: RuntimeBase::LibraryRoot,
            path: RelativePath::new(&"q".repeat(final_path_bytes)).unwrap(),
        });
    let bytes = value.encode().unwrap();
    assert_eq!(bytes.len(), MAX_METADATA_BYTES);
    assert!(RuntimeRuleMetadata::decode(&bytes).is_ok());
    value.rules[0].runtime.paths[0]
        .candidates
        .last_mut()
        .unwrap()
        .path = RelativePath::new(&"q".repeat(final_path_bytes + 1)).unwrap();
    assert!(value.encode().unwrap_err().to_string().contains("1 MiB"));
    let mut oversized = bytes;
    oversized.push(0);
    assert!(
        RuntimeRuleMetadata::decode(&oversized)
            .unwrap_err()
            .to_string()
            .contains("1 MiB")
    );
    assert!(
        EntrypointSets::decode(&oversized)
            .unwrap_err()
            .to_string()
            .contains("1 MiB")
    );
}

#[test]
fn noncanonical_selection_and_rule_order_are_rejected() {
    let value = EntrypointSets {
        pins: vec![pin("a", 1), pin("b", 2)],
        entrypoints: vec![EntrypointSelection {
            name: "app".into(),
            sets: vec![id("a"), id("b")],
        }],
    };
    let original = value.encode().unwrap();
    let mut bytes = original.clone();
    let first_pin_end = 20 + 4 + get_u32(&bytes, 20);
    let second_pin_end = first_pin_end + 4 + get_u32(&bytes, first_pin_end);
    let first = bytes[20..first_pin_end].to_vec();
    let second = bytes[first_pin_end..second_pin_end].to_vec();
    bytes[20..first_pin_end].copy_from_slice(&second);
    bytes[first_pin_end..second_pin_end].copy_from_slice(&first);
    assert!(EntrypointSets::decode(&bytes).is_err());
    let mut bytes = original;
    let length = bytes.len();
    bytes.swap(length - 1, length - 6);
    assert!(EntrypointSets::decode(&bytes).is_err());
    let mut value = RuntimeRuleMetadata {
        rules: vec![
            RuntimeRuleRecord {
                id: id("a"),
                binding: None,
                runtime: RuntimeRules::default(),
            },
            RuntimeRuleRecord {
                id: id("b"),
                binding: None,
                runtime: RuntimeRules::default(),
            },
        ],
    };
    let mut bytes = value.encode().unwrap();
    let first_end = 20 + 4 + get_u32(&bytes, 20);
    let first = bytes[20..first_end].to_vec();
    let second = bytes[first_end..].to_vec();
    bytes[20..first_end].copy_from_slice(&second);
    bytes[first_end..].copy_from_slice(&first);
    assert!(RuntimeRuleMetadata::decode(&bytes).is_err());
    value.rules[1].id = id("a");
    assert!(value.encode().is_err());
}

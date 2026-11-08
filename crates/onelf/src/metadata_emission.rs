//! Metadata lowering, capability gating, emission and packing-gate tests.
//!
//! Lowering turns validated runtime mappings into the two encoded records
//! a package can carry, the recipe build path lowers the selection table
//! its resolved selections write through it, the capability note gates
//! what a package may carry, and the packer validates a carried selection
//! table against the final entrypoint table and the pins the tree records
//! before anything is written. A build whose recipe writes no explicit
//! selection lowers nothing, so it packs byte-identically to before.

use crate::{assembly, bundle, compress, info, metadata, pack, recipe};

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use onelf_format::elf::{
    NOTE_NAME, NOTE_TYPE, RUNTIME_METADATA_VERSION, RuntimeCapabilities, RuntimeNote,
    probe_capability_note,
};
use onelf_format::rules::{
    EnvironmentName, Inheritance, PathSelection, RelativePath, ResourceKind, RuleId, RuntimeBase,
    RuntimeCandidate, RuntimeHookId, RuntimePathGroup, RuntimeRules,
};
use onelf_format::runtime_metadata::{
    EntrypointSelection, EntrypointSets, RuntimeRuleMetadata, RuntimeRuleRecord, SetPin,
};

/// A temp directory unique to one test and call, so parallel tests never
/// share state.
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "onelf-metadata-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Seed `root` with the app tree every pack in this file starts from:
/// `bin/app`, the command both the direct packs and the recipes name,
/// inside the directory the packer walks or the recipe's directory names.
fn seed_app(root: &Path) {
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::write(root.join("bin/app"), b"#!/bin/sh\necho hi\n").unwrap();
    fs::set_permissions(root.join("bin/app"), PermissionsExt::from_mode(0o755)).unwrap();
}

/// One presence-based mapping: a package-root directory exported as one
/// environment variable.
fn mapping(id: &str, path: &str, variable: &str) -> RuntimePathGroup {
    RuntimePathGroup {
        id: RuleId::new(id).unwrap(),
        kind: ResourceKind::Directory,
        candidates: vec![RuntimeCandidate {
            base: RuntimeBase::PackageRoot,
            path: RelativePath::new(path).unwrap(),
        }],
        variables: vec![EnvironmentName::new(variable).unwrap()],
        selection: PathSelection::First,
        inheritance: Inheritance::Preserve,
    }
}

/// One rule record with a path group and one closed hook.
fn record(id: &str, binding: Option<SetPin>) -> RuntimeRuleRecord {
    RuntimeRuleRecord {
        id: RuleId::new(id).unwrap(),
        binding,
        runtime: RuntimeRules {
            paths: vec![mapping("paths", "share/data", "APP_DATA_DIRS")],
            hooks: vec![RuntimeHookId::FontconfigGenerate],
        },
    }
}

/// A pin table with one declared set and one entrypoint selecting it.
fn sets_table(name: &str, hash: [u8; 32]) -> EntrypointSets {
    EntrypointSets {
        pins: vec![SetPin {
            name: RuleId::new(name).unwrap(),
            hash,
        }],
        entrypoints: vec![EntrypointSelection {
            name: "app".to_string(),
            sets: vec![RuleId::new(name).unwrap()],
        }],
    }
}

/// A runtime advertising exactly what the embedded builds advertise:
/// payload metadata version 1 and the selection view.
fn capable_note() -> RuntimeNote {
    RuntimeNote::Present {
        metadata_version: RUNTIME_METADATA_VERSION,
        capabilities: RuntimeCapabilities::SELECTION_VIEW,
    }
}

/// Record declared sets into `root`'s `.onelf/sets`, the record the
/// packer checks a carried pin table against. Each entry is a set name
/// and the pin its build is published under.
fn record_declared_sets(root: &Path, sets: &[(&str, [u8; 32])]) {
    let declared: Vec<bundle::sysroot::SharedSet> = sets
        .iter()
        .map(|(name, hash)| bundle::sysroot::SharedSet {
            name: (*name).to_string(),
            packages: Vec::new(),
            url: format!("https://e/{name}.onelf"),
            blake3: blake3::Hash::from(*hash).to_hex().to_string(),
        })
        .collect();
    bundle::sysroot::record_sets(root, &declared, None).unwrap();
}

/// A runtime that parses payload metadata but resolves no selections: the
/// shape of an older embedded build.
fn note_without_view() -> RuntimeNote {
    RuntimeNote::Present {
        metadata_version: RUNTIME_METADATA_VERSION,
        capabilities: RuntimeCapabilities::empty(),
    }
}

/// An ELF64 carrying one onelf capability note whose description cannot
/// decode, the shape of a runtime whose advertisement is unreadable.
fn incompatible_note() -> RuntimeNote {
    const PT_NOTE: u32 = 4;
    let mut elf = vec![0u8; 152];
    elf[0..4].copy_from_slice(b"\x7fELF");
    elf[4] = 2;
    elf[32..40].copy_from_slice(&64u64.to_le_bytes());
    elf[54..56].copy_from_slice(&56u16.to_le_bytes());
    elf[56..58].copy_from_slice(&1u16.to_le_bytes());
    elf[64..68].copy_from_slice(&PT_NOTE.to_le_bytes());
    elf[72..80].copy_from_slice(&120u64.to_le_bytes());
    elf[96..104].copy_from_slice(&32u64.to_le_bytes());
    elf[120..124].copy_from_slice(&(NOTE_NAME.len() as u32 + 1).to_le_bytes());
    elf[124..128].copy_from_slice(&12u32.to_le_bytes());
    elf[128..132].copy_from_slice(&NOTE_TYPE.to_le_bytes());
    elf[132..132 + NOTE_NAME.len()].copy_from_slice(NOTE_NAME);
    // A description naming the vendor and type but reserving a nonzero
    // field, which the decoder refuses.
    elf[140..144].copy_from_slice(b"OCAP");
    elf[144..146].copy_from_slice(&1u16.to_le_bytes());
    elf[146..148].copy_from_slice(&1u16.to_le_bytes());
    let note = probe_capability_note(&elf).unwrap();
    match note {
        RuntimeNote::Incompatible(_) => note,
        other => panic!("the crafted note probed {other:?}, not incompatible"),
    }
}

/// A packable set of options over the seeded app tree, carrying no
/// metadata and a capable runtime note unless the test replaces either.
fn test_opts(app: &Path, out: &Path, note: RuntimeNote) -> pack::PackOptions {
    pack::PackOptions {
        directory: app.to_path_buf(),
        output: out.to_path_buf(),
        command: "bin/app".to_string(),
        name: None,
        entrypoints: Vec::new(),
        default_entrypoint: None,
        lib_dirs: Vec::new(),
        level: 3,
        block_size: compress::DEFAULT_BLOCK_SIZE,
        payload: pack::Payload::Blocks,
        use_dict: false,
        no_compress: false,
        memfd: Some(false),
        working_dir: onelf_format::WorkingDir::Inherit,
        host_libs: pack::HostLibs::default(),
        cache: false,
        prefer_cache: false,
        update_url: None,
        embed_updater: false,
        update_key: None,
        update_revision: None,
        update_unsigned: false,
        exclude: Vec::new(),
        package_info: None,
        mtime: Some(0),
        env: Vec::new(),
        paths: Vec::new(),
        inferred_pairs: Vec::new(),
        preload: Vec::new(),
        needs_setuid: false,
        profile: Vec::new(),
        provider_index: None,
        metadata: pack::MetadataPayload::default(),
        runtime_note: note,
    }
}

#[test]
fn lowered_rules_round_trip_through_the_codec() {
    let records = vec![
        record("alsa", None),
        record(
            "gtk",
            Some(SetPin {
                name: RuleId::new("gtk").unwrap(),
                hash: [7; 32],
            }),
        ),
    ];
    let payload =
        pack::lower_runtime_metadata(records.clone(), Some(sets_table("gtk", [7; 32]))).unwrap();
    let rules = RuntimeRuleMetadata::decode(payload.runtime_rules.as_deref().unwrap()).unwrap();
    // Encoding sorts scoped identities, None before Some.
    let mut sorted = records;
    sorted.sort_by_key(|rule| (rule.id.clone(), rule.binding.clone()));
    assert_eq!(rules.rules, sorted);
    let sets = EntrypointSets::decode(payload.entrypoint_sets.as_deref().unwrap()).unwrap();
    assert_eq!(sets, sets_table("gtk", [7; 32]));
}

#[test]
fn lowered_bytes_are_canonical_and_stable() {
    let reversed = vec![record("zzz", None), record("aaa", None)];
    let payload = pack::lower_runtime_metadata(reversed, None).unwrap();
    let bytes = payload.runtime_rules.clone().unwrap();
    let decoded = RuntimeRuleMetadata::decode(&bytes).unwrap();
    assert_eq!(
        decoded
            .rules
            .iter()
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
        ["aaa", "zzz"],
        "decoding must observe the canonical identity order"
    );
    assert_eq!(decoded.encode().unwrap(), bytes, "decoding re-encodes 1:1");
    let again =
        pack::lower_runtime_metadata(vec![record("aaa", None), record("zzz", None)], None).unwrap();
    assert_eq!(again.runtime_rules, payload.runtime_rules);
}

#[test]
fn lowering_refuses_duplicates_and_unknown_pins() {
    let duplicate =
        pack::lower_runtime_metadata(vec![record("same", None), record("same", None)], None)
            .unwrap_err();
    assert!(
        duplicate
            .to_string()
            .contains("duplicate scoped descriptor identity"),
        "{duplicate}"
    );
    let err = pack::lower_runtime_metadata(
        vec![record(
            "bound",
            Some(SetPin {
                name: RuleId::new("elsewhere").unwrap(),
                hash: [1; 32],
            }),
        )],
        Some(sets_table("gtk", [2; 32])),
    )
    .unwrap_err();
    assert!(err.to_string().contains("runtime binding"), "{err}");
}

#[test]
fn nothing_to_lower_emits_nothing() {
    assert_eq!(
        pack::lower_runtime_metadata(Vec::new(), None).unwrap(),
        pack::MetadataPayload::default(),
        "an ordinary recipe with no rule declarations carries no records"
    );
    // An explicit empty selection table is a statement, not absence.
    let explicit = pack::lower_runtime_metadata(
        Vec::new(),
        Some(EntrypointSets {
            pins: vec![],
            entrypoints: vec![EntrypointSelection {
                name: "app".to_string(),
                sets: vec![],
            }],
        }),
    )
    .unwrap();
    assert!(explicit.runtime_rules.is_none());
    assert!(explicit.entrypoint_sets.is_some());
}

#[test]
fn custom_rule_mappings_lower_without_their_sources() {
    let custom = "local-frobnicator";
    assert!(
        !onelf_format::catalog::builtins()
            .iter()
            .any(|d| d.id.as_str() == custom),
        "the fixture must not name a built-in rule"
    );
    let payload = pack::lower_runtime_metadata(vec![record(custom, None)], None).unwrap();
    let decoded = RuntimeRuleMetadata::decode(payload.runtime_rules.as_deref().unwrap()).unwrap();
    assert_eq!(decoded.rules.len(), 1);
    assert_eq!(decoded.rules[0].id.as_str(), custom);
    decoded.validate().unwrap();
}

#[test]
fn a_capable_runtime_carries_the_lowered_records() {
    let root = temp_dir("capable");
    seed_app(&root);
    let out = root.join("pkg.onelf");
    let mut opts = test_opts(&root, &out, capable_note());
    opts.metadata = pack::lower_runtime_metadata(vec![record("alsa", None)], None).unwrap();
    pack::pack(&opts, b"stub-runtime").unwrap();
    let bytes = metadata::read_packed_file(&out, ".onelf/runtime-rules")
        .unwrap()
        .expect("the rules record is carried");
    let decoded = RuntimeRuleMetadata::decode(&bytes).unwrap();
    assert_eq!(decoded.rules[0].id.as_str(), "alsa");
    assert!(
        metadata::read_packed_file(&out, ".onelf/entrypoint-sets")
            .unwrap()
            .is_none()
    );
}

#[test]
fn selections_need_the_selection_view_and_are_carried_when_advertised() {
    let root = temp_dir("view");
    seed_app(&root);
    record_declared_sets(&root, &[("gtk", [9; 32])]);
    let payload =
        pack::lower_runtime_metadata(Vec::new(), Some(sets_table("gtk", [9; 32]))).unwrap();
    let out = root.join("pkg.onelf");
    let refused = pack::pack(
        &{
            let mut opts = test_opts(&root, &out, note_without_view());
            opts.metadata = payload.clone();
            opts
        },
        b"stub-runtime",
    )
    .unwrap_err();
    let text = refused.to_string();
    assert!(text.contains("refusing to embed a runtime"), "{text}");
    assert!(text.contains("selection-view"), "{text}");
    assert!(!out.exists(), "the refusal must come before any writing");

    let out = root.join("pkg.onelf");
    let mut opts = test_opts(&root, &out, capable_note());
    opts.metadata = payload;
    pack::pack(&opts, b"stub-runtime").unwrap();
    let bytes = metadata::read_packed_file(&out, ".onelf/entrypoint-sets")
        .unwrap()
        .expect("the selection table is carried");
    assert_eq!(
        EntrypointSets::decode(&bytes).unwrap(),
        sets_table("gtk", [9; 32])
    );
}

#[test]
fn rules_alone_need_only_the_metadata_version() {
    let root = temp_dir("rules-only");
    seed_app(&root);
    let out = root.join("pkg.onelf");
    let mut opts = test_opts(&root, &out, note_without_view());
    opts.metadata = pack::lower_runtime_metadata(vec![record("alsa", None)], None).unwrap();
    pack::pack(&opts, b"stub-runtime").unwrap();
    assert!(
        metadata::read_packed_file(&out, ".onelf/runtime-rules")
            .unwrap()
            .is_some()
    );
}

#[test]
fn an_absent_note_passes_only_when_nothing_is_emitted() {
    let root = temp_dir("absent");
    seed_app(&root);
    let out = root.join("legacy.onelf");
    pack::pack(
        &test_opts(&root, &out, RuntimeNote::Absent),
        b"stub-runtime",
    )
    .unwrap();
    assert!(
        metadata::read_packed_file(&out, ".onelf/runtime-rules")
            .unwrap()
            .is_none()
    );

    let out = root.join("refused.onelf");
    let refused = pack::pack(
        &{
            let mut opts = test_opts(&root, &out, RuntimeNote::Absent);
            opts.metadata = pack::lower_runtime_metadata(vec![record("alsa", None)], None).unwrap();
            opts
        },
        b"stub-runtime",
    )
    .unwrap_err();
    let text = refused.to_string();
    assert!(text.contains("refusing to embed a runtime"), "{text}");
    assert!(text.contains("no capability note"), "{text}");
    assert!(text.contains("metadata version 1"), "{text}");
    assert!(!out.exists());
}

#[test]
fn an_unreadable_note_is_refused_even_with_nothing_to_emit() {
    let root = temp_dir("incompatible");
    seed_app(&root);
    let out = root.join("pkg.onelf");
    let refused = pack::pack(
        &test_opts(&root, &out, incompatible_note()),
        b"stub-runtime",
    )
    .unwrap_err();
    assert!(refused.to_string().contains("capability note"), "{refused}");
    assert!(!out.exists());
}

#[test]
fn a_skipped_tree_carries_records_without_discovery() {
    let root = temp_dir("skip");
    seed_app(&root);
    let out = root.join("pkg.onelf");
    let recipe_path = root.join("onelf.toml");
    let hash = [0xab; 32];
    fs::write(
        &recipe_path,
        format!(
            "[package]\ncommand = \"bin/app\"\nmtime = 0\n\n[bundle]\nskip = true\n\n\
             [sysroot.sets.gtk]\npackages = [\"gtk3\"]\nurl = \"https://e/gtk.onelf\"\n\
             blake3 = \"{}\"\n",
            blake3::Hash::from(hash).to_hex()
        ),
    )
    .unwrap();
    let recipe = recipe::load(&recipe_path).unwrap();
    let request =
        assembly::AssemblyRequest::plan(&recipe_path, recipe, Some(out.as_path())).unwrap();
    let report = request.stage().unwrap();
    assert_eq!(report.source, assembly::Source::Unchecked);
    assert!(report.active_rules.is_empty(), "skip runs no discovery");
    assert_eq!(
        request
            .pack_options(None, RuntimeNote::Absent, Vec::new())
            .metadata,
        pack::MetadataPayload::default(),
        "an omitted selection lowers no records"
    );

    let runtime = assembly::embed_runtime(&request).unwrap();
    let payload = pack::lower_runtime_metadata(
        vec![record(
            "gtk",
            Some(SetPin {
                name: RuleId::new("gtk").unwrap(),
                hash,
            }),
        )],
        Some(sets_table("gtk", hash)),
    )
    .unwrap();
    let mut opts = request.pack_options(None, runtime.note, Vec::new());
    opts.metadata = payload;
    pack::pack(&opts, runtime.bytes).unwrap();

    let rules = metadata::read_packed_file(&out, ".onelf/runtime-rules")
        .unwrap()
        .expect("the skipped tree carries the record");
    let decoded = RuntimeRuleMetadata::decode(&rules).unwrap();
    assert_eq!(decoded.rules[0].id.as_str(), "gtk");
    let sets = metadata::read_packed_file(&out, ".onelf/entrypoint-sets")
        .unwrap()
        .expect("the skipped tree carries the table");
    decoded
        .validate_bindings(&EntrypointSets::decode(&sets).unwrap())
        .unwrap();
}

#[test]
fn a_minimal_recipe_emits_no_metadata_records() {
    let root = temp_dir("minimal");
    seed_app(&root);
    let out = root.join("pkg.onelf");
    let recipe_path = root.join("onelf.toml");
    fs::write(&recipe_path, "[package]\ncommand = \"bin/app\"\n").unwrap();
    let recipe = recipe::load(&recipe_path).unwrap();
    let request =
        assembly::AssemblyRequest::plan(&recipe_path, recipe, Some(out.as_path())).unwrap();
    request.stage().unwrap();
    let runtime = assembly::embed_runtime(&request).unwrap();
    let opts = request.pack_options(None, runtime.note, Vec::new());
    assert_eq!(opts.metadata, pack::MetadataPayload::default());
    pack::pack(&opts, runtime.bytes).unwrap();
    let (_, manifest) = info::read_footer_and_manifest(&out).unwrap();
    assert!(metadata::find_entry_by_path(&manifest, ".onelf/runtime-rules").is_none());
    assert!(metadata::find_entry_by_path(&manifest, ".onelf/entrypoint-sets").is_none());
}

/// The packer checks a carried pin table against the pins the tree
/// records: a pin that does not match the actual published set fails
/// the pack before anything is written, and the matching table passes.
#[test]
fn a_mismatched_pin_table_is_refused_before_writing() {
    let root = temp_dir("pin-mismatch");
    seed_app(&root);
    record_declared_sets(&root, &[("gtk", [9; 32])]);
    let wrong = pack::lower_runtime_metadata(Vec::new(), Some(sets_table("gtk", [7; 32]))).unwrap();
    let out = root.join("pkg.onelf");
    let refused = pack::pack(
        &{
            let mut opts = test_opts(&root, &out, capable_note());
            opts.metadata = wrong;
            opts
        },
        b"stub-runtime",
    )
    .unwrap_err();
    let text = refused.to_string();
    assert!(text.contains("does not match the package"), "{text}");
    assert!(text.contains("pin table"), "{text}");
    assert!(!out.exists(), "a refused pin table writes nothing");

    let matching =
        pack::lower_runtime_metadata(Vec::new(), Some(sets_table("gtk", [9; 32]))).unwrap();
    let mut opts = test_opts(&root, &out, capable_note());
    opts.metadata = matching;
    pack::pack(&opts, b"stub-runtime").unwrap();
    assert!(
        metadata::read_packed_file(&out, ".onelf/entrypoint-sets")
            .unwrap()
            .is_some()
    );
}

/// A carried selection table must be complete over the final manifest
/// entrypoint names: a record naming an entrypoint the package does not
/// carry is refused before anything is written.
#[test]
fn a_record_off_the_final_entrypoint_table_is_refused() {
    let root = temp_dir("name-mismatch");
    seed_app(&root);
    record_declared_sets(&root, &[("gtk", [9; 32])]);
    let stray = EntrypointSets {
        pins: vec![SetPin {
            name: RuleId::new("gtk").unwrap(),
            hash: [9; 32],
        }],
        entrypoints: vec![EntrypointSelection {
            name: "other".to_string(),
            sets: vec![RuleId::new("gtk").unwrap()],
        }],
    };
    let payload = pack::lower_runtime_metadata(Vec::new(), Some(stray)).unwrap();
    let out = root.join("pkg.onelf");
    let refused = pack::pack(
        &{
            let mut opts = test_opts(&root, &out, capable_note());
            opts.metadata = payload;
            opts
        },
        b"stub-runtime",
    )
    .unwrap_err();
    let text = refused.to_string();
    assert!(text.contains("does not match the package"), "{text}");
    assert!(text.contains("entrypoint names"), "{text}");
    assert!(!out.exists());
}

/// Tampered or truncated carried bytes are refused at pack level: the
/// packer decodes what it is about to write, so a payload that fails the
/// shared codec fails the build instead of shipping as a package the
/// runtime must reject at launch.
#[test]
fn truncated_carried_metadata_is_refused_before_writing() {
    let root = temp_dir("truncated");
    seed_app(&root);
    record_declared_sets(&root, &[("gtk", [9; 32])]);
    let mut bytes = pack::lower_runtime_metadata(Vec::new(), Some(sets_table("gtk", [9; 32])))
        .unwrap()
        .entrypoint_sets
        .unwrap();
    bytes.truncate(bytes.len() - 1);
    let out = root.join("pkg.onelf");
    let refused = pack::pack(
        &{
            let mut opts = test_opts(&root, &out, capable_note());
            opts.metadata.entrypoint_sets = Some(bytes);
            opts
        },
        b"stub-runtime",
    )
    .unwrap_err();
    let text = refused.to_string();
    assert!(text.contains("runtime metadata"), "{text}");
    assert!(!out.exists());
}

/// A selection naming a set outside the pin table cannot be lowered, a
/// carried table over a tree that declares no pins has nothing to
/// validate against, and an empty carried table matches no manifest.
#[test]
fn selections_without_declared_pins_are_refused() {
    let ghost = EntrypointSets {
        pins: vec![SetPin {
            name: RuleId::new("gtk").unwrap(),
            hash: [9; 32],
        }],
        entrypoints: vec![EntrypointSelection {
            name: "app".to_string(),
            sets: vec![RuleId::new("ghost").unwrap()],
        }],
    };
    let err = pack::lower_runtime_metadata(Vec::new(), Some(ghost)).unwrap_err();
    assert!(
        err.to_string().contains("unknown declared set name"),
        "{err}"
    );

    let root = temp_dir("no-pins");
    seed_app(&root);
    let out = root.join("pkg.onelf");
    let payload =
        pack::lower_runtime_metadata(Vec::new(), Some(sets_table("gtk", [9; 32]))).unwrap();
    let refused = pack::pack(
        &{
            let mut opts = test_opts(&root, &out, capable_note());
            opts.metadata = payload;
            opts
        },
        b"stub-runtime",
    )
    .unwrap_err();
    assert!(
        refused.to_string().contains("does not match the package"),
        "{refused}"
    );
    assert!(
        !out.exists(),
        "a tree declaring no pins carries no pin table"
    );

    let empty = pack::lower_runtime_metadata(
        Vec::new(),
        Some(EntrypointSets {
            pins: Vec::new(),
            entrypoints: Vec::new(),
        }),
    )
    .unwrap();
    let out = root.join("empty.onelf");
    let refused = pack::pack(
        &{
            let mut opts = test_opts(&root, &out, capable_note());
            opts.metadata = empty;
            opts
        },
        b"stub-runtime",
    )
    .unwrap_err();
    assert!(
        refused.to_string().contains("does not match the package"),
        "{refused}"
    );
    assert!(!out.exists());
}

/// A skipped unchecked tree carries its declarations and nothing more:
/// the pin table is exactly what `.onelf/sets` records, the selections
/// answer to the final entrypoint names, and no rule record is lowered
/// for a tree nothing audited.
#[test]
fn an_unchecked_tree_carries_declarations_and_no_satisfied_providers() {
    let root = temp_dir("unchecked");
    seed_app(&root);
    let out = root.join("pkg.onelf");
    let hash = [0x5a; 32];
    let recipe_path = root.join("onelf.toml");
    fs::write(
        &recipe_path,
        format!(
            "[package]\ncommand = \"bin/app\"\nmtime = 0\nsets = [\"gtk\"]\n\n[bundle]\nskip = true\n\n\
             [sysroot.sets.gtk]\npackages = [\"gtk3\"]\nurl = \"https://e/gtk.onelf\"\n\
             blake3 = \"{}\"\n",
            blake3::Hash::from(hash).to_hex()
        ),
    )
    .unwrap();
    let recipe = recipe::load(&recipe_path).unwrap();
    let request =
        assembly::AssemblyRequest::plan(&recipe_path, recipe, Some(out.as_path())).unwrap();
    let report = request.stage().unwrap();
    assert_eq!(report.source, assembly::Source::Unchecked);

    let payload = pack::lower_runtime_metadata(Vec::new(), Some(sets_table("gtk", hash))).unwrap();
    let runtime = assembly::embed_runtime(&request).unwrap();
    let mut opts = request.pack_options(None, runtime.note, Vec::new());
    opts.metadata = payload;
    pack::pack(&opts, runtime.bytes).unwrap();

    let carried = EntrypointSets::decode(
        &metadata::read_packed_file(&out, ".onelf/entrypoint-sets")
            .unwrap()
            .expect("the unchecked tree carries its declarations"),
    )
    .unwrap();
    let text = fs::read_to_string(root.join(".onelf/sets")).unwrap();
    let value: toml::Value = toml::from_str(&text).unwrap();
    let recorded: Vec<SetPin> = value
        .as_table()
        .unwrap()
        .into_iter()
        .map(|(name, set)| SetPin {
            name: RuleId::new(name).unwrap(),
            hash: *blake3::Hash::from_hex(set.get("blake3").unwrap().as_str().unwrap())
                .unwrap()
                .as_bytes(),
        })
        .collect();
    carried
        .validate_against(&["app"], &recorded)
        .expect("the record is the declared table, nothing more");
    assert!(
        metadata::read_packed_file(&out, ".onelf/runtime-rules")
            .unwrap()
            .is_none(),
        "nothing audited lowers no rule record"
    );
}

/// Primary and named selections encode canonically and round-trip: the
/// primary under its manifest name, named entrypoints under theirs, a
/// renamed entrypoint keeping its recipe name.
#[test]
fn primary_and_named_selections_round_trip() {
    let gtk = RuleId::new("gtk").unwrap();
    let record = EntrypointSets {
        pins: vec![SetPin {
            name: gtk.clone(),
            hash: [3; 32],
        }],
        entrypoints: vec![
            EntrypointSelection {
                name: "app".to_string(),
                sets: vec![gtk.clone()],
            },
            EntrypointSelection {
                name: "cli".to_string(),
                sets: Vec::new(),
            },
        ],
    };
    let bytes = record.encode().unwrap();
    let decoded = EntrypointSets::decode(&bytes).unwrap();
    assert_eq!(decoded, record, "the record round-trips whole");
    assert_eq!(decoded.encode().unwrap(), bytes, "decoding re-encodes 1:1");
    // The canonical order holds regardless of the order the table was
    // written in.
    let reordered = EntrypointSets {
        pins: record.pins.clone(),
        entrypoints: record.entrypoints.iter().rev().cloned().collect(),
    };
    assert_eq!(reordered.encode().unwrap(), bytes);
}

/// The legacy entrypoint record size is kept: a package carrying
/// selection metadata still serializes every entrypoint in 14 bytes,
/// the record's own name bound applies to the metadata alone, and a
/// legacy build over a name past that bound packs as it always did.
#[test]
fn legacy_entrypoint_size_is_kept() {
    let long = "l".repeat(129);
    let root = temp_dir("ep-size");
    seed_app(&root);
    record_declared_sets(&root, &[("gtk", [9; 32])]);

    let payload = pack::lower_runtime_metadata(
        Vec::new(),
        Some(EntrypointSets {
            pins: vec![SetPin {
                name: RuleId::new("gtk").unwrap(),
                hash: [9; 32],
            }],
            entrypoints: vec![EntrypointSelection {
                name: long.clone(),
                sets: vec![RuleId::new("gtk").unwrap()],
            }],
        }),
    );
    let err = payload.unwrap_err();
    assert!(err.to_string().contains("1..=128"), "{err}");

    let legacy_out = root.join("legacy.onelf");
    let mut legacy = test_opts(&root, &legacy_out, RuntimeNote::Absent);
    legacy.entrypoints = vec![(long.clone(), "bin/app".to_string(), Vec::new())];
    pack::pack(&legacy, b"stub-runtime").unwrap();
    assert!(
        metadata::read_packed_file(&legacy_out, ".onelf/entrypoint-sets")
            .unwrap()
            .is_none(),
        "a legacy build over the same name carries no record"
    );

    let out = root.join("carrying.onelf");
    let mut opts = test_opts(&root, &out, capable_note());
    opts.metadata = pack::lower_runtime_metadata(
        Vec::new(),
        Some(EntrypointSets {
            pins: vec![SetPin {
                name: RuleId::new("gtk").unwrap(),
                hash: [9; 32],
            }],
            entrypoints: vec![EntrypointSelection {
                name: "app".to_string(),
                sets: vec![RuleId::new("gtk").unwrap()],
            }],
        }),
    )
    .unwrap();
    pack::pack(&opts, b"stub-runtime").unwrap();
    let (_, manifest) = info::read_footer_and_manifest(&out).unwrap();
    for entrypoint in &manifest.entrypoints {
        let mut written = Vec::new();
        entrypoint.write_to(&mut written).unwrap();
        assert_eq!(
            written.len(),
            onelf_format::entry::ENTRYPOINT_SIZE,
            "the entrypoint record stays 14 bytes beside the metadata"
        );
    }
}

#[test]
fn an_https_pin_refuses_a_runtime_without_the_store_import_advertisement() {
    let note = RuntimeNote::Present {
        metadata_version: RUNTIME_METADATA_VERSION,
        capabilities: RuntimeCapabilities::SELECTION_VIEW,
    };
    let refused = pack::require_store_runtime(&note, true).unwrap_err();
    let text = refused.to_string();
    assert!(text.contains("store import"), "{text}");
    assert!(text.contains("HTTPS"), "{text}");
}

#[test]
fn an_https_pin_accepts_a_store_importing_runtime() {
    let note = RuntimeNote::Present {
        metadata_version: RUNTIME_METADATA_VERSION,
        capabilities: RuntimeCapabilities::SELECTION_VIEW | RuntimeCapabilities::STORE_IMPORT,
    };
    pack::require_store_runtime(&note, true).unwrap();
}

#[test]
fn file_based_pins_never_demand_store_import() {
    pack::require_store_runtime(&RuntimeNote::Absent, false).unwrap();
}

/// Anchored paths resolve only through a capable runtime's descriptor, so
/// a record refuses a runtime that does not advertise them, and a
/// malformed or empty record is refused before anything is packed.
#[test]
fn anchors_require_a_runtime_advertising_them() {
    let capable = RuntimeNote::Present {
        metadata_version: RUNTIME_METADATA_VERSION,
        capabilities: RuntimeCapabilities::SELECTION_VIEW | RuntimeCapabilities::ANCHORS,
    };
    let record: &[u8] = b"a\tshare/alsa";
    let anchors = pack::require_anchor_runtime(&capable, record).unwrap();
    assert_eq!(anchors.len(), 1);
    for incapable in [capable_note(), RuntimeNote::Absent] {
        let text = pack::require_anchor_runtime(&incapable, record)
            .unwrap_err()
            .to_string();
        assert!(text.contains("anchor support"), "{text}");
    }
    let text = pack::require_anchor_runtime(&capable, b"")
        .unwrap_err()
        .to_string();
    assert!(text.contains("is empty"), "{text}");
    let text = pack::require_anchor_runtime(&capable, b"a\t../out")
        .unwrap_err()
        .to_string();
    assert!(text.contains(".onelf/anchors"), "{text}");
}

/// A freshly built set-carrying package launches on the current runtime:
/// the slim embedded runtime passes its own gates, the selected set is
/// acquired from the file-pinned store, the unselected set is never
/// fetched, and the empty-selection entrypoint still runs.
#[test]
fn a_set_carrying_build_launches_on_the_current_runtime() {
    let root = temp_dir("launch");
    let (qt_src, gtk_src) = (root.join("qt-src"), root.join("gtk-src"));
    fs::create_dir_all(qt_src.join("lib")).unwrap();
    fs::create_dir_all(gtk_src.join("lib")).unwrap();
    fs::write(qt_src.join("lib/libqt.so.5"), b"qt fixture payload\n").unwrap();
    fs::write(gtk_src.join("lib/libgtk-3.so.0"), b"gtk fixture payload\n").unwrap();

    let build_set_artifact = |src: &Path, out: &Path| {
        seed_app(src);
        let opts = test_opts(src, out, RuntimeNote::Absent);
        pack::pack(&opts, b"set-build-prefix").unwrap();
        let bytes = fs::read(out).unwrap();
        blake3::hash(&bytes).to_hex().to_string()
    };
    let (qt_hex, gtk_hex) = (
        build_set_artifact(&qt_src, &qt_src.join("qt.onelf")),
        build_set_artifact(&gtk_src, &gtk_src.join("gtk.onelf")),
    );

    seed_app(&root);
    fs::write(root.join("bin/app"), b"#!/bin/sh\nexit 7\n").unwrap();
    let out = root.join("pkg.onelf");
    let recipe_path = root.join("onelf.toml");
    fs::write(
        &recipe_path,
        format!(
            "[package]\ncommand = \"bin/app\"\nmtime = 0\nsets = [\"qt\"]\n\n\
             [bundle]\nskip = true\n\n\
             [sysroot.sets.qt]\npackages = [\"qt5\"]\nurl = \"file://{}\"\nblake3 = \"{}\"\n\n\
             [sysroot.sets.gtk]\npackages = [\"gtk3\"]\nurl = \"file://{}\"\nblake3 = \"{}\"\n\n\
             [[entrypoint]]\nname = \"cli\"\npath = \"bin/app\"\nargs = [\"--cli\"]\nsets = []\n",
            qt_src.join("qt.onelf").display(),
            qt_hex,
            gtk_src.join("gtk.onelf").display(),
            gtk_hex,
        ),
    )
    .unwrap();
    let recipe = recipe::load(&recipe_path).unwrap();
    let request =
        assembly::AssemblyRequest::plan(&recipe_path, recipe, Some(out.as_path())).unwrap();
    request.stage().unwrap();

    let runtime = assembly::embed_runtime(&request).unwrap();
    let payload = pack::lower_runtime_metadata(
        Vec::new(),
        Some(EntrypointSets {
            pins: vec![
                SetPin {
                    name: RuleId::new("qt").unwrap(),
                    hash: hash_from_hex(&qt_hex),
                },
                SetPin {
                    name: RuleId::new("gtk").unwrap(),
                    hash: hash_from_hex(&gtk_hex),
                },
            ],
            entrypoints: vec![
                EntrypointSelection {
                    name: "app".to_string(),
                    sets: vec![RuleId::new("qt").unwrap()],
                },
                EntrypointSelection {
                    name: "cli".to_string(),
                    sets: Vec::new(),
                },
            ],
        }),
    )
    .unwrap();
    let mut opts = request.pack_options(None, runtime.note, Vec::new());
    opts.metadata = payload;
    pack::pack(&opts, runtime.bytes).unwrap();

    let store = root.join("store");
    let cache = root.join("cache");
    fs::set_permissions(out.as_path(), PermissionsExt::from_mode(0o755)).unwrap();
    let result = Command::new(&out)
        .env_remove("ONELF_ENTRYPOINT")
        .env_remove("ONELF_ENTRYPOINT_EXPORTED")
        .env_remove("ONELF_CACHE")
        .env_remove("ONELF_NO_PLATFORM_FETCH")
        .env("ONELF_MODE", "rundir")
        .env("ONELF_SET_MODE", "extract")
        .env("ONELF_NO_RESOLVER", "1")
        .env("ONELF_PLATFORM_STORE", &store)
        .env("XDG_CACHE_HOME", &cache)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&result.stderr).into_owned();
    assert!(
        result.status.code() == Some(7),
        "the app ran to its own exit: {:?}\nstderr: {stderr}",
        result.status
    );
    assert!(
        store.join("qt").is_dir(),
        "the selected set was acquired: {:?}",
        fs::read_dir(&store).map(|entries| entries.count())
    );
    assert!(
        !store.join("gtk").exists(),
        "the unselected set must never be acquired"
    );
}

/// The blake3 digest a pin table carries, parsed back from its hex form.
fn hash_from_hex(hex: &str) -> [u8; 32] {
    *blake3::Hash::from_hex(hex).unwrap().as_bytes()
}

/// `onelf info` explains a record-carrying package's pin table, each
/// entrypoint's selection, and the metadata capabilities the package
/// requires beside what the runtime it carries advertises. The findings
/// stay empty while the records agree, and the whole explanation reads
/// the package's bytes alone.
#[test]
fn info_explains_selections_and_capabilities_without_acquiring() {
    let root = temp_dir("info-explains");
    seed_app(&root);
    let out = root.join("pkg.onelf");
    let recipe_path = root.join("onelf.toml");
    let (qt_hex, gtk_hex) = ("1".repeat(64), "2".repeat(64));
    fs::write(
        &recipe_path,
        format!(
            "[package]\ncommand = \"bin/app\"\nmtime = 0\nsets = [\"qt\"]\n\n\
             [bundle]\nskip = true\n\n\
             [sysroot.sets.qt]\npackages = [\"qt5\"]\nurl = \"https://e/qt.onelf\"\nblake3 = \"{qt_hex}\"\n\n\
             [sysroot.sets.gtk]\npackages = [\"gtk3\"]\nurl = \"https://e/gtk.onelf\"\nblake3 = \"{gtk_hex}\"\n"
        ),
    )
    .unwrap();
    let recipe = recipe::load(&recipe_path).unwrap();
    let request =
        assembly::AssemblyRequest::plan(&recipe_path, recipe, Some(out.as_path())).unwrap();
    request.stage().unwrap();
    let runtime = assembly::embed_runtime(&request).unwrap();
    let mut opts = request.pack_options(None, runtime.note, Vec::new());
    opts.metadata = pack::lower_runtime_metadata(
        Vec::new(),
        Some(EntrypointSets {
            pins: vec![
                SetPin {
                    name: RuleId::new("qt").unwrap(),
                    hash: hash_from_hex(&qt_hex),
                },
                SetPin {
                    name: RuleId::new("gtk").unwrap(),
                    hash: hash_from_hex(&gtk_hex),
                },
            ],
            entrypoints: vec![EntrypointSelection {
                name: "app".to_string(),
                sets: vec![RuleId::new("qt").unwrap()],
            }],
        }),
    )
    .unwrap();
    pack::pack(&opts, runtime.bytes).unwrap();

    let (footer, manifest) = info::read_footer_and_manifest(&out).unwrap();
    let selections = info::selection_lines(&out, &footer, &manifest)
        .unwrap()
        .join("\n");
    assert!(selections.contains("Declared pins:"), "{selections}");
    assert!(
        selections.contains(&format!("  qt: pinned {qt_hex}, from https://e/qt.onelf")),
        "{selections}"
    );
    assert!(
        selections.contains(&format!(
            "  gtk: pinned {gtk_hex}, from https://e/gtk.onelf"
        )),
        "{selections}"
    );
    assert!(selections.contains("Selections:"), "{selections}");
    assert!(selections.contains("  app (default): qt"), "{selections}");
    assert!(
        selections.contains("Declared but selected by no entrypoint: gtk"),
        "{selections}"
    );
    assert!(
        !selections.contains("Findings:"),
        "agreeing records name no conflict:\n{selections}"
    );

    let capabilities = info::capability_lines(&out, &footer, &manifest)
        .unwrap()
        .join("\n");
    assert!(
        capabilities.contains("Records:     entrypoint-sets"),
        "{capabilities}"
    );
    assert!(
        capabilities.contains("Requires:    payload metadata version 1, selection-view"),
        "{capabilities}"
    );
    assert!(
        capabilities.contains("advertises payload metadata version 1, selection-view"),
        "{capabilities}"
    );
    assert!(
        capabilities.contains("Agreement:   satisfied"),
        "{capabilities}"
    );
}

/// A package without metadata records explains nothing: no sets section
/// and no runtime metadata section. A legacy package keeps its
/// documented defaults.
#[test]
fn info_stays_silent_for_a_legacy_package() {
    let root = temp_dir("info-legacy");
    seed_app(&root);
    let out = root.join("pkg.onelf");
    pack::pack(
        &test_opts(&root, &out, RuntimeNote::Absent),
        b"stub-runtime",
    )
    .unwrap();
    let (footer, manifest) = info::read_footer_and_manifest(&out).unwrap();
    assert!(
        info::selection_lines(&out, &footer, &manifest)
            .unwrap()
            .is_empty()
    );
    assert!(
        info::capability_lines(&out, &footer, &manifest)
            .unwrap()
            .is_empty()
    );
}

/// The conflicts `onelf info` can state from the records alone, without
/// reading any artifact: a set declared more than once, a record whose
/// pins do not match the pin table, and a record that does not match
/// the manifest's entrypoints. Every line is sorted, so the report is a
/// function of the records.
#[test]
fn info_reports_the_conflicts_the_records_reveal() {
    let (one, two, three) = ("11".repeat(32), "22".repeat(32), "33".repeat(32));
    let table = format!(
        "[qt]\nurl = \"https://e/qt.onelf\"\nblake3 = \"{one}\"\n\
         [qt]\nurl = \"https://e/other.onelf\"\nblake3 = \"{two}\"\n\
         [gtk]\nurl = \"https://e/gtk.onelf\"\nblake3 = \"{three}\"\n"
    );
    let declared = onelf_rt::sets::parse(&table);
    assert_eq!(declared.len(), 3, "a repeated name stays visible");
    assert_eq!(declared[1].blake3, two, "the second declaration is kept");

    let stray = EntrypointSets {
        pins: vec![SetPin {
            name: RuleId::new("qt").unwrap(),
            hash: hash_from_hex(&one),
        }],
        entrypoints: vec![EntrypointSelection {
            name: "other".to_string(),
            sets: vec![RuleId::new("qt").unwrap()],
        }],
    };
    let findings = info::selection_findings(&declared, Some(&Ok(stray)), &["app"]);
    let joined = findings.join("\n");
    assert!(
        joined.contains("the set 'qt' is declared more than once"),
        "{joined}"
    );
    assert!(
        joined.contains("the selection table does not match the package's entrypoints"),
        "{joined}"
    );
    assert!(
        joined.contains("the pin table declares 'gtk', which the selection record does not carry"),
        "{joined}"
    );

    let mismatched = EntrypointSets {
        pins: vec![SetPin {
            name: RuleId::new("qt").unwrap(),
            hash: hash_from_hex(&"44".repeat(32)),
        }],
        entrypoints: vec![EntrypointSelection {
            name: "app".to_string(),
            sets: vec![RuleId::new("qt").unwrap()],
        }],
    };
    let findings = info::selection_findings(&declared, Some(&Ok(mismatched)), &["app"]);
    assert!(
        findings.iter().any(|f| f
            .contains("the pin for 'qt' differs between the pin table and the selection record")),
        "{findings:?}"
    );
}

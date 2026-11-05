//! Versioned payload metadata gating an application launch.
//!
//! A package may carry two regular payload records beside its payload:
//! `.onelf/runtime-rules` (normalized runtime path groups and closed hook
//! IDs) and `.onelf/entrypoint-sets` (the canonical pin table and each
//! entrypoint's named-set selection). Both are ordinary manifest entries
//! read through their content hashes, bounded by their declared sizes, and
//! decoded by the shared codec, which rejects truncated, unsupported,
//! noncanonical or internally inconsistent bytes before returning.
//!
//! Reading both records here, before any dispatch decision, is the launch
//! gate. A package with neither record keeps the legacy defaults: every
//! declared set, no runtime rule overrides. A present record that fails to
//! read, decode, or agree with its counterpart ends the launch before the
//! memfd read, set acquisition, hook registration, or any environment
//! change. Present but broken is never read as absence.

use std::io;

use onelf_format::runtime_metadata::{EntrypointSets, MAX_METADATA_BYTES, RuntimeRuleMetadata};

use crate::loader::{self, PackageData};
use crate::platform::Pin;
use crate::sets::SETS_FILE;

/// Where the package carries its validated runtime rule record.
pub const RUNTIME_RULES_FILE: &str = ".onelf/runtime-rules";

/// Where the package carries its canonical pin table and entrypoint
/// selections.
pub const ENTRYPOINT_SETS_FILE: &str = ".onelf/entrypoint-sets";

/// What the launch gate decoded: the runtime rule record a launch
/// evaluates, and the pins the invoked entrypoint selects.
#[derive(Debug)]
pub struct LaunchMetadata {
    /// The carried runtime rules, or `None` for the built-in fallback.
    pub rules: Option<RuntimeRuleMetadata>,
    /// The pins `entrypoint` acquires, in selection order.
    pub pins: Vec<Pin>,
}

/// Read, decode and cross-validate both metadata records ahead of every
/// dispatch decision, and return the carried rules with the pins
/// `entrypoint` selects.
///
/// Each record is checked on its own when the other is absent. The
/// rules' set-pin bindings must all name pins the selection table
/// declares, or, without a selection record, pins the set table
/// declares. A selection record is also checked against the manifest's
/// entrypoint names and the set table's actual pins, so a record the
/// package cannot back ends even a memfd launch before it executes
/// anything.
pub fn validate(pkg: &PackageData, entrypoint: &str) -> io::Result<LaunchMetadata> {
    let rules = runtime_rules(pkg)?;
    let sets = entrypoint_sets(pkg)?;
    let table = loader::read_bounded_record(pkg, SETS_FILE, MAX_METADATA_BYTES)?;
    if let Some(rules) = &rules {
        match &sets {
            Some(sets) => rules.validate_bindings(sets).map_err(rejected)?,
            None if rules.rules.iter().any(|r| r.binding.is_some()) => {
                let declared = crate::sets::declared_pins(table.as_deref())
                    .map_err(|why| io::Error::new(io::ErrorKind::InvalidData, why))?;
                rules
                    .validate_bindings(&EntrypointSets {
                        pins: declared,
                        entrypoints: Vec::new(),
                    })
                    .map_err(rejected)?;
            }
            None => {}
        }
    }
    let pins = crate::sets::selection(table.as_deref(), sets.as_ref(), &pkg.manifest, entrypoint)
        .map_err(|why| io::Error::new(io::ErrorKind::InvalidData, why))?;
    Ok(LaunchMetadata { rules, pins })
}

/// The package's runtime rule record, or `None` for the legacy default.
fn runtime_rules(pkg: &PackageData) -> io::Result<Option<RuntimeRuleMetadata>> {
    match loader::read_bounded_record(pkg, RUNTIME_RULES_FILE, MAX_METADATA_BYTES)? {
        None => Ok(None),
        Some(bytes) => RuntimeRuleMetadata::decode(&bytes)
            .map_err(rejected)
            .map(Some),
    }
}

/// The package's selection record, or `None` for the legacy default.
fn entrypoint_sets(pkg: &PackageData) -> io::Result<Option<EntrypointSets>> {
    match loader::read_bounded_record(pkg, ENTRYPOINT_SETS_FILE, MAX_METADATA_BYTES)? {
        None => Ok(None),
        Some(bytes) => EntrypointSets::decode(&bytes).map_err(rejected).map(Some),
    }
}

/// A record the launch refuses, carrying the decoder's bounded diagnostic.
fn rejected(why: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("runtime metadata: {why}"),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs::File;
    use std::path::PathBuf;
    use std::process::Command;

    use onelf_format::rules::{
        EnvironmentName, Inheritance, PathSelection, RelativePath, ResourceKind, RuleId,
        RuntimeBase, RuntimeCandidate, RuntimeHookId, RuntimePathGroup, RuntimeRules,
    };
    use onelf_format::runtime_metadata::{
        EntrypointSelection, MAX_METADATA_BYTES, RuntimeRuleMetadata, RuntimeRuleRecord,
    };
    use onelf_format::{
        Block, Entry, EntryKind, EntryPoint, EntryPointFlags, Flags, Footer, Manifest,
        ManifestHeader, WorkingDir,
    };

    use crate::loader;
    use crate::sets::SETS_FILE;

    use super::{ENTRYPOINT_SETS_FILE, RUNTIME_RULES_FILE, validate};

    const PIN_A: [u8; 32] = [0xaa; 32];
    const PIN_B: [u8; 32] = [0xbb; 32];

    /// A fixture package loaded through the real loader, with its file
    /// removed when the case goes away.
    struct Case {
        pkg: loader::PackageData,
        path: PathBuf,
    }

    impl Drop for Case {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn case(name: &str, files: &[(&str, Vec<u8>)]) -> Case {
        let path =
            std::env::temp_dir().join(format!("onelf-pkgmeta-{name}-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let built = fixture::build(b"onelf fixture prefix", files, &[("app", 0)]);
        std::fs::write(&path, &built.bytes).unwrap();
        let pkg = loader::load_file(File::open(&path).unwrap()).unwrap();
        Case { pkg, path }
    }

    fn path_groups() -> Vec<RuntimePathGroup> {
        vec![RuntimePathGroup {
            id: RuleId::new("mime-data").unwrap(),
            kind: ResourceKind::Directory,
            candidates: vec![RuntimeCandidate {
                base: RuntimeBase::PackageRoot,
                path: RelativePath::new("share/mime").unwrap(),
            }],
            variables: vec![EnvironmentName::new("XDG_DATA_DIRS").unwrap()],
            selection: PathSelection::First,
            inheritance: Inheritance::Preserve,
        }]
    }

    fn rules_for(binding: Option<onelf_format::runtime_metadata::SetPin>) -> RuntimeRuleMetadata {
        RuntimeRuleMetadata {
            rules: vec![RuntimeRuleRecord {
                id: RuleId::new("shared-mime").unwrap(),
                binding,
                runtime: RuntimeRules {
                    paths: path_groups(),
                    hooks: vec![RuntimeHookId::FontconfigGenerate],
                },
            }],
        }
    }

    fn pin(name: &str, hash: [u8; 32]) -> onelf_format::runtime_metadata::SetPin {
        onelf_format::runtime_metadata::SetPin {
            name: RuleId::new(name).unwrap(),
            hash,
        }
    }

    fn sets_bytes(pins: &[(&str, [u8; 32])], entries: &[(&str, &[&str])]) -> Vec<u8> {
        onelf_format::runtime_metadata::EntrypointSets {
            pins: pins.iter().map(|(n, h)| pin(n, *h)).collect(),
            entrypoints: entries
                .iter()
                .map(|(name, sets)| EntrypointSelection {
                    name: name.to_string(),
                    sets: sets.iter().map(|s| RuleId::new(s).unwrap()).collect(),
                })
                .collect(),
        }
        .encode()
        .unwrap()
    }

    fn bound_rule_bytes(hash: [u8; 32]) -> Vec<u8> {
        rules_for(Some(pin("qt", hash))).encode().unwrap()
    }

    fn plain_rule_bytes() -> Vec<u8> {
        rules_for(None).encode().unwrap()
    }

    fn sets_record() -> Vec<u8> {
        let hash = "aa".repeat(32);
        format!("[qt]\nurl = \"file:///nonexistent/qt.onelf\"\nblake3 = \"{hash}\"\n").into_bytes()
    }

    #[test]
    fn a_package_without_records_keeps_the_legacy_defaults() {
        let case = case("plain", &[("bin/app", b"elf bytes".to_vec())]);
        assert!(validate(&case.pkg, "app").is_ok());
    }

    #[test]
    fn valid_records_pass_the_launch_gate() {
        let case = case(
            "valid",
            &[
                ("bin/app", b"elf bytes".to_vec()),
                (SETS_FILE, sets_record()),
                (RUNTIME_RULES_FILE, bound_rule_bytes(PIN_A)),
                (
                    ENTRYPOINT_SETS_FILE,
                    sets_bytes(&[("qt", PIN_A)], &[("app", &["qt"])]),
                ),
            ],
        );
        assert!(validate(&case.pkg, "app").is_ok());
    }

    #[test]
    fn either_record_alone_still_validates() {
        let rules = case("lone-rules", &[(RUNTIME_RULES_FILE, plain_rule_bytes())]);
        assert!(validate(&rules.pkg, "app").is_ok());
        let sets = case(
            "lone-sets",
            &[
                (SETS_FILE, sets_record()),
                (
                    ENTRYPOINT_SETS_FILE,
                    sets_bytes(&[("qt", PIN_A)], &[("app", &["qt"])]),
                ),
            ],
        );
        assert!(validate(&sets.pkg, "app").is_ok());
    }

    #[test]
    fn a_binding_outside_the_pin_table_fails_cross_validation() {
        let case = case(
            "binding",
            &[
                (RUNTIME_RULES_FILE, bound_rule_bytes(PIN_B)),
                (
                    ENTRYPOINT_SETS_FILE,
                    sets_bytes(&[("qt", PIN_A)], &[("app", &["qt"])]),
                ),
            ],
        );
        let err = validate(&case.pkg, "app").unwrap_err();
        assert!(err.to_string().contains("runtime binding"), "{err}");
    }

    /// Without a selection record, a binding is checked against the set
    /// table itself: a pin the table declares passes and the decoded rules
    /// come back for the launch to evaluate, while any other pin fails.
    #[test]
    fn a_binding_is_checked_against_the_set_table_without_a_selection_record() {
        let bound = case(
            "table-binding",
            &[
                (SETS_FILE, sets_record()),
                (RUNTIME_RULES_FILE, bound_rule_bytes(PIN_A)),
            ],
        );
        let meta = validate(&bound.pkg, "app").unwrap();
        assert_eq!(meta.rules, Some(rules_for(Some(pin("qt", PIN_A)))));
        assert_eq!(meta.pins.len(), 1);

        let stray = case(
            "table-stray",
            &[
                (SETS_FILE, sets_record()),
                (RUNTIME_RULES_FILE, bound_rule_bytes(PIN_B)),
            ],
        );
        let err = validate(&stray.pkg, "app").unwrap_err();
        assert!(err.to_string().contains("runtime binding"), "{err}");
    }

    #[test]
    fn a_truncated_record_is_rejected() {
        let encoded = sets_bytes(&[("qt", PIN_A)], &[("app", &["qt"])]);
        let truncated = encoded[..encoded.len() - 1].to_vec();
        let case = case("truncated", &[(ENTRYPOINT_SETS_FILE, truncated)]);
        let err = validate(&case.pkg, "app").unwrap_err();
        assert!(err.to_string().contains("runtime metadata"), "{err}");
    }

    #[test]
    fn an_unsupported_version_or_magic_is_rejected() {
        let encoded = sets_bytes(&[("qt", PIN_A)], &[("app", &["qt"])]);
        let mut version = encoded.clone();
        version[4] = 2;
        let wrong_version = case("version", &[(ENTRYPOINT_SETS_FILE, version)]);
        let err = validate(&wrong_version.pkg, "app").unwrap_err();
        assert!(
            err.to_string().contains("unsupported metadata version"),
            "{err}"
        );

        let mut magic = encoded;
        magic[0] = b'X';
        let wrong_magic = case("magic", &[(ENTRYPOINT_SETS_FILE, magic)]);
        let err = validate(&wrong_magic.pkg, "app").unwrap_err();
        assert!(err.to_string().contains("invalid metadata magic"), "{err}");
    }

    #[test]
    fn a_noncanonical_record_is_rejected() {
        let encoded = sets_bytes(
            &[("aaa", PIN_A), ("bbb", PIN_B)],
            &[("app", &["aaa", "bbb"])],
        );
        // Each pin record is 44 bytes: length, tag, name string, hash. The
        // two records sit back to back after the header and the pin count,
        // so swapping the spans reverses the canonical name order while
        // leaving every record intact.
        let mut swapped = encoded.clone();
        let (first, second) = (swapped[20..64].to_vec(), swapped[64..108].to_vec());
        swapped[20..64].copy_from_slice(&second);
        swapped[64..108].copy_from_slice(&first);
        let case = case("noncanonical", &[(ENTRYPOINT_SETS_FILE, swapped)]);
        let err = validate(&case.pkg, "app").unwrap_err();
        assert!(err.to_string().contains("noncanonical"), "{err}");
    }

    #[test]
    fn a_tampered_record_is_rejected_by_its_content_hash() {
        let oset = sets_bytes(&[("qt", PIN_A)], &[("app", &["qt"])]);
        let built = fixture::build(
            b"onelf fixture prefix",
            &[
                ("bin/app", b"elf bytes".to_vec()),
                (ENTRYPOINT_SETS_FILE, oset),
            ],
            &[],
        );
        let mut bytes = built.bytes;
        let (start, _) = built.spans[1];
        bytes[start] ^= 0xff;
        let path =
            std::env::temp_dir().join(format!("onelf-pkgmeta-tampered-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, &bytes).unwrap();
        let pkg = loader::load_file(File::open(&path).unwrap()).unwrap();
        let err = validate(&pkg, "app").unwrap_err();
        assert!(err.to_string().contains("hash mismatch"), "{err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_record_over_the_size_cap_is_refused_before_reading() {
        let mut case = case(
            "cap",
            &[(
                ENTRYPOINT_SETS_FILE,
                sets_bytes(&[("qt", PIN_A)], &[("app", &["qt"])]),
            )],
        );
        let idx = loader::find_file_entry(&case.pkg, ENTRYPOINT_SETS_FILE).unwrap();
        case.pkg.manifest.entries[idx].blocks[0].original_size = MAX_METADATA_BYTES as u64 + 1;
        let err = validate(&case.pkg, "app").unwrap_err();
        assert!(err.to_string().contains("record limit"), "{err}");
    }

    #[test]
    fn a_present_unreadable_record_is_not_treated_as_absence() {
        let mut case = case(
            "unreadable",
            &[(
                ENTRYPOINT_SETS_FILE,
                sets_bytes(&[("qt", PIN_A)], &[("app", &["qt"])]),
            )],
        );
        let idx = loader::find_file_entry(&case.pkg, ENTRYPOINT_SETS_FILE).unwrap();
        case.pkg.manifest.entries[idx].blocks[0].payload_offset = u64::MAX;
        assert!(validate(&case.pkg, "app").is_err());
    }

    /// The built runtime's bytes: the executable prefix every packed
    /// fixture runs as.
    fn runtime_bytes() -> Vec<u8> {
        let from_cargo = std::env::var("CARGO_BIN_EXE_onelf-rt")
            .ok()
            .map(PathBuf::from);
        let from_test_exe = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.ancestors().nth(2).map(|dir| dir.join("onelf-rt")));
        let path = from_cargo
            .or(from_test_exe)
            .unwrap_or_else(|| panic!("cannot locate the built onelf-rt binary"));
        std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    /// Run a fixture as a packed executable: the built runtime is the
    /// prefix, the fixture is its package, and the child's exit code and
    /// stderr report how far the launch got. Acquisition cannot succeed
    /// here, so "dependency set" on stderr is the observable on the far
    /// side of the gate.
    fn launch(name: &str, package: &fixture::Package) -> (Option<i32>, String) {
        let _forks = crate::test_locks::forking();
        let dir = std::env::temp_dir().join(format!(
            "onelf-pkgmeta-launch-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("app");
        std::fs::write(&exe, &package.bytes).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let store = dir.join("store");
        let output = Command::new(&exe)
            .env_remove("ONELF_MODE")
            .env_remove("ONELF_ENTRYPOINT_EXPORTED")
            .env_remove("ONELF_CACHE")
            .env("ONELF_ENTRYPOINT", "app")
            .env("ONELF_NO_PLATFORM_FETCH", "1")
            .env("ONELF_PLATFORM_STORE", &store)
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    fn launch_fixture(name: &str, records: &[(&str, Vec<u8>)]) -> (Option<i32>, String) {
        // The fixture's prefix is the runtime itself: its manifest and
        // payload offsets are only correct in a file that runs as this
        // runtime with the package appended.
        let runtime = runtime_bytes();
        let mut files: Vec<(&str, Vec<u8>)> = vec![("bin/app", b"elf bytes".to_vec())];
        files.extend_from_slice(records);
        let package = fixture::build(&runtime, &files, &[("app", 0)]);
        launch(name, &package)
    }

    #[test]
    fn invalid_metadata_ends_the_launch_before_set_acquisition() {
        let oset = sets_bytes(&[("qt", PIN_A)], &[("app", &["qt"])]);
        let mut tampered = oset.clone();
        tampered[20] ^= 0xff;
        let (code, stderr) = launch_fixture(
            "gate-tampered",
            &[(SETS_FILE, sets_record()), (ENTRYPOINT_SETS_FILE, tampered)],
        );
        assert_eq!(code, Some(1), "{stderr}");
        assert!(stderr.contains("runtime metadata"), "{stderr}");
        assert!(!stderr.contains("dependency set"), "{stderr}");
    }

    #[test]
    fn valid_metadata_reaches_set_acquisition() {
        let (code, stderr) = launch_fixture(
            "gate-valid",
            &[
                (SETS_FILE, sets_record()),
                (
                    ENTRYPOINT_SETS_FILE,
                    sets_bytes(&[("qt", PIN_A)], &[("app", &["qt"])]),
                ),
            ],
        );
        assert_eq!(code, Some(1), "{stderr}");
        assert!(stderr.contains("dependency set"), "{stderr}");
    }

    #[test]
    fn absent_metadata_reaches_set_acquisition_like_a_legacy_package() {
        let (code, stderr) = launch_fixture("gate-absent", &[(SETS_FILE, sets_record())]);
        assert_eq!(code, Some(1), "{stderr}");
        assert!(stderr.contains("dependency set"), "{stderr}");
    }

    /// A memfd-eligible entrypoint needs no set, yet a selection record
    /// the package cannot back still stops it before it executes. The
    /// consistent record shows the same entrypoint does run in memfd.
    #[test]
    fn an_inconsistent_selection_stops_a_memfd_entrypoint_before_it_runs() {
        let runtime = runtime_bytes();
        let script = b"#!/bin/sh\necho MEMFD-RAN >&2\nexit 42\n".to_vec();
        let package = |record: Vec<u8>| {
            fixture::build_flagged(
                &runtime,
                &[
                    ("bin/app", script.clone()),
                    (SETS_FILE, sets_record()),
                    (ENTRYPOINT_SETS_FILE, record),
                ],
                &[("app", 0)],
                EntryPointFlags::MEMFD_ELIGIBLE,
            )
        };

        let stale = sets_bytes(&[("qt", PIN_A)], &[("app", &["qt"]), ("ghost", &[])]);
        let (code, stderr) = launch("gate-memfd-stale", &package(stale));
        assert_eq!(code, Some(1), "{stderr}");
        assert!(!stderr.contains("MEMFD-RAN"), "{stderr}");
        assert!(stderr.contains("entrypoint-sets"), "{stderr}");

        let consistent = sets_bytes(&[("qt", PIN_A)], &[("app", &["qt"])]);
        let (code, stderr) = launch("gate-memfd-ok", &package(consistent));
        assert_eq!(code, Some(42), "{stderr}");
        assert!(stderr.contains("MEMFD-RAN"), "{stderr}");
    }

    /// Minimal stored-mode packages the runtime loader can open.
    ///
    /// The layout is `[prefix][manifest][stored payload][footer]`: one
    /// block per file, bytes verbatim, so a fixture payload offset is also
    /// a byte offset a test can tamper with.
    pub(crate) mod fixture {
        use super::*;

        /// A built package and the absolute byte span of each file's
        /// stored payload, in input order.
        pub(crate) struct Package {
            pub(crate) bytes: Vec<u8>,
            pub(crate) spans: Vec<(usize, usize)>,
        }

        /// Build a package carrying `files`, whose entrypoints are
        /// `(name, target entry index)` pairs.
        pub(crate) fn build(
            prefix: &[u8],
            files: &[(&str, Vec<u8>)],
            entrypoints: &[(&str, u32)],
        ) -> Package {
            build_flagged(prefix, files, entrypoints, EntryPointFlags::empty())
        }

        /// [`build`] with every entrypoint carrying `flags`.
        pub(crate) fn build_flagged(
            prefix: &[u8],
            files: &[(&str, Vec<u8>)],
            entrypoints: &[(&str, u32)],
            flags: EntryPointFlags,
        ) -> Package {
            let mut string_table: Vec<u8> = vec![0];
            let mut name_offsets = Vec::new();
            for (name, _) in files {
                name_offsets.push(string_table.len() as u32);
                string_table.extend_from_slice(name.as_bytes());
                string_table.push(0);
            }
            let ep_names: Vec<u32> = entrypoints
                .iter()
                .map(|(name, _)| {
                    let offset = string_table.len() as u32;
                    string_table.extend_from_slice(name.as_bytes());
                    string_table.push(0);
                    offset
                })
                .collect();

            let mut payload = Vec::new();
            let mut relative = Vec::new();
            let mut entries = Vec::new();
            for (i, (_, content)) in files.iter().enumerate() {
                relative.push((payload.len(), content.len()));
                payload.extend_from_slice(content);
                let hash = *blake3::hash(content).as_bytes();
                entries.push(Entry {
                    kind: EntryKind::File,
                    parent: u32::MAX,
                    name: name_offsets[i],
                    mode: 0o755,
                    mtime_secs: 0,
                    mtime_nsec: 0,
                    content_hash: hash,
                    blocks: vec![Block {
                        payload_offset: relative[i].0 as u64,
                        compressed_size: content.len() as u64,
                        original_size: content.len() as u64,
                        content_hash: hash,
                    }],
                    symlink_target: 0,
                });
            }
            let eps: Vec<EntryPoint> = entrypoints
                .iter()
                .zip(ep_names)
                .map(|((_, target), name)| EntryPoint {
                    name,
                    target_entry: *target,
                    args: 0,
                    working_dir: WorkingDir::Inherit,
                    flags,
                })
                .collect();
            let manifest = Manifest {
                header: ManifestHeader {
                    version: 3,
                    entry_count: entries.len() as u32,
                    string_table_size: string_table.len() as u32,
                    entrypoint_count: eps.len() as u16,
                    default_entrypoint: 0,
                    lib_dir_count: 0,
                    name_offset: 0,
                    package_id: [0; 32],
                    prefix_hash: *blake3::hash(prefix).as_bytes(),
                    footer_hash: [0; 32],
                },
                entrypoints: eps,
                entries,
                lib_dir_offsets: Vec::new(),
                string_table,
            };
            let raw = manifest.serialize().unwrap();
            let compressed = zstd::bulk::compress(&raw, 1).unwrap();
            let manifest_offset = prefix.len() as u64;
            let payload_offset = manifest_offset + compressed.len() as u64;
            let footer = Footer {
                format_version: onelf_format::footer::FORMAT_VERSION,
                flags: Flags::STORED,
                manifest_offset,
                manifest_compressed: compressed.len() as u64,
                manifest_original: raw.len() as u64,
                payload_offset,
                payload_size: payload.len() as u64,
                dict_offset: 0,
                dict_size: 0,
                manifest_checksum: onelf_format::manifest::manifest_checksum(&raw).unwrap(),
            };
            let mut bytes = Vec::new();
            bytes.extend_from_slice(prefix);
            bytes.extend_from_slice(&compressed);
            bytes.extend_from_slice(&payload);
            footer.write_to(&mut bytes).unwrap();
            let base = manifest_offset as usize + compressed.len();
            let spans = relative
                .into_iter()
                .map(|(start, len)| (base + start, len))
                .collect();
            Package { bytes, spans }
        }
    }
}

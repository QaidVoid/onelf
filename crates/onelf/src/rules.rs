//! The framework rule catalog one assembly uses: the built-in descriptors,
//! the project-local descriptors a recipe selects with `[bundle] rules`,
//! and the identities it disables with `[bundle] disable-rules`.
//!
//! Local descriptor files are parsed with the shared TOML adapter and
//! validated together with the built-ins before anything in the AppDir
//! changes. A local rule extends the catalog and never replaces a
//! built-in identity, even a disabled one. The same catalog drives
//! discovery and lowers the runtime records a package carries, so a
//! packed launch needs none of the descriptor files.

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use onelf_format::catalog;
use onelf_format::rules::{self, Descriptor, MAX_DESCRIPTOR_BYTES, RuleId};
use onelf_format::runtime_metadata::{RuntimeRuleRecord, SetPin};

use crate::bundle::PreparedPin;

/// A validated rule catalog: the local descriptors in recipe order and the
/// disabled identities. The default is the built-in catalog alone.
#[derive(Debug, Clone, Default)]
pub struct RuleCatalog {
    local: Vec<Descriptor>,
    disabled: Vec<RuleId>,
}

impl RuleCatalog {
    /// Load and validate the descriptor `files` and `disabled` identities a
    /// recipe at `recipe_path` names. Relative files resolve against
    /// `dir`, the recipe directory.
    ///
    /// Every failure names the recipe and, for a descriptor, its file and
    /// identity: a parse or schema error, a local identity that shadows a
    /// built-in or repeats another local file's, and an unknown or repeated
    /// disabled identity. Nothing is written.
    pub fn load(
        recipe_path: &Path,
        dir: &Path,
        files: &[PathBuf],
        disabled: &[String],
    ) -> io::Result<Self> {
        let invalid = |what: String| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: {what}", recipe_path.display()),
            )
        };
        let disabled = parse_disabled(disabled).map_err(invalid)?;
        let mut local: Vec<Descriptor> = Vec::with_capacity(files.len());
        let mut sources: Vec<PathBuf> = Vec::with_capacity(files.len());
        for file in files {
            let path = dir.join(file);
            let descriptor = read_descriptor(&path)
                .map_err(|e| invalid(format!("[bundle] rules {}: {e}", path.display())))?;
            let id = descriptor.id.as_str().to_string();
            if catalog::builtins().iter().any(|b| b.id == descriptor.id) {
                return Err(invalid(format!(
                    "[bundle] rules {}: rule {id} shadows the built-in rule of that \
                     identity, which a local rule may never replace, even when it is \
                     disabled. Give the local rule an identity of its own and disable \
                     the built-in with disable-rules",
                    path.display()
                )));
            }
            if let Some(at) = local.iter().position(|d| d.id == descriptor.id) {
                return Err(invalid(format!(
                    "[bundle] rules {}: rule {id} repeats the identity {} already \
                     declares",
                    path.display(),
                    sources[at].display()
                )));
            }
            local.push(descriptor);
            sources.push(path);
            let known: Vec<RuleId> = disabled
                .iter()
                .filter(|id| {
                    catalog::builtins().iter().any(|b| b.id == **id)
                        || local.iter().any(|d| d.id == **id)
                })
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            rules::validate_catalog(catalog::builtins(), &local, &known).map_err(|e| {
                invalid(format!(
                    "[bundle] rules {}: rule {id}: {e}",
                    sources.last().unwrap().display()
                ))
            })?;
        }
        rules::validate_catalog(catalog::builtins(), &local, &disabled)
            .map_err(|e| invalid(format!("[bundle] disable-rules: {e}")))?;
        Ok(Self { local, disabled })
    }

    /// The project-local descriptors, in recipe order.
    pub fn local(&self) -> &[Descriptor] {
        &self.local
    }

    /// The disabled identities, in recipe order.
    pub fn disabled(&self) -> &[RuleId] {
        &self.disabled
    }

    /// The descriptor of `id`, built-in or local, whether or not it is
    /// enabled.
    pub fn find(&self, id: &str) -> Option<&Descriptor> {
        catalog::builtins()
            .iter()
            .chain(&self.local)
            .find(|d| d.id.as_str() == id)
    }

    /// The enabled descriptors, built-in and local, sorted by identity.
    pub fn enabled(&self) -> Vec<&Descriptor> {
        let mut enabled: Vec<&Descriptor> = catalog::builtins()
            .iter()
            .chain(&self.local)
            .filter(|d| !self.disabled.contains(&d.id))
            .collect();
        enabled.sort_by(|a, b| a.id.cmp(&b.id));
        enabled
    }

    /// The runtime records a package carries, or `None` when the runtime's
    /// built-in fallback already reproduces them exactly.
    ///
    /// Every enabled rule with runtime paths or hooks contributes one
    /// unbound record, so a carried record replaces the fallback whole. Each
    /// pin a selected set's preparation wrote contributes one bound record
    /// per rule that prepared into it, carrying that rule's runtime paths,
    /// which a launch evaluates against the pin's subtree only while that
    /// pin is selected. With no binding, the record is needed only when a
    /// local rule or a disabled built-in makes the enabled runtime behavior
    /// differ from the built-ins, so a recipe without either carries no
    /// record and packs exactly as before.
    pub fn runtime_records(
        &self,
        prepared: &[PreparedPin],
    ) -> io::Result<Option<Vec<RuntimeRuleRecord>>> {
        let unbound = |d: &Descriptor| RuntimeRuleRecord {
            id: d.id.clone(),
            binding: None,
            runtime: d.runtime.clone(),
        };
        let has_runtime =
            |d: &&Descriptor| !d.runtime.paths.is_empty() || !d.runtime.hooks.is_empty();
        let mut records: Vec<RuntimeRuleRecord> = self
            .enabled()
            .into_iter()
            .filter(has_runtime)
            .map(unbound)
            .collect();
        let fallback: Vec<RuntimeRuleRecord> = catalog::builtins()
            .iter()
            .filter(has_runtime)
            .map(unbound)
            .collect();
        let mut bound = false;
        for pin in prepared {
            let binding = SetPin {
                name: RuleId::set_name(&pin.set).map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, format!("set {}: {e}", pin.set))
                })?,
                hash: *blake3::Hash::from_hex(&pin.pin)
                    .map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("set {}: pin is not a blake3 digest", pin.set),
                        )
                    })?
                    .as_bytes(),
            };
            for rule in &pin.rules {
                let Some(descriptor) = self.find(rule) else {
                    continue;
                };
                if descriptor.runtime.paths.is_empty() || self.disabled.contains(&descriptor.id) {
                    continue;
                }
                records.push(RuntimeRuleRecord {
                    id: descriptor.id.clone(),
                    binding: Some(binding.clone()),
                    runtime: rules::RuntimeRules {
                        paths: descriptor.runtime.paths.clone(),
                        hooks: Vec::new(),
                    },
                });
                bound = true;
            }
        }
        Ok((bound || records != fallback).then_some(records))
    }
}

/// Read one descriptor file, bounded before parsing, through the shared
/// TOML adapter, which validates it before returning.
fn read_descriptor(path: &Path) -> io::Result<Descriptor> {
    let mut text = String::new();
    std::fs::File::open(path)?
        .take(MAX_DESCRIPTOR_BYTES as u64 + 1)
        .read_to_string(&mut text)?;
    onelf_format::rules_toml::parse_descriptor(&text)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

/// The disabled identities as rule identities, refusing malformed ones.
fn parse_disabled(disabled: &[String]) -> Result<Vec<RuleId>, String> {
    disabled
        .iter()
        .map(|id| RuleId::new(id).map_err(|e| format!("[bundle] disable-rules {id:?}: {e}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const VENDOR: &str = r#"schema = 1
id = "vendor-framework"

[[triggers]]
soname-prefix = "libvendor.so"

[[resources.plugins]]
id = "plugins"
kind = "directory"
destination = "lib/vendor-plugins"
requirement = "optional"
candidates = [{ base = "library-dir", path = "vendor-plugins" }]

[[runtime.paths]]
id = "plugins"
kind = "directory"
variables = ["VENDOR_PLUGIN_DIR"]
selection = "first"
inheritance = "replace"
candidates = [{ base = "library-root", path = "vendor-plugins" }]
"#;

    fn dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("onelf-rules-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn load(dir: &Path, files: &[(&str, &str)], disabled: &[&str]) -> io::Result<RuleCatalog> {
        for (name, text) in files {
            std::fs::write(dir.join(name), text).unwrap();
        }
        let names: Vec<PathBuf> = files.iter().map(|(n, _)| PathBuf::from(n)).collect();
        let disabled: Vec<String> = disabled.iter().map(|s| s.to_string()).collect();
        RuleCatalog::load(&dir.join("onelf.toml"), dir, &names, &disabled)
    }

    fn failure(result: io::Result<RuleCatalog>) -> String {
        result.unwrap_err().to_string()
    }

    #[test]
    fn a_local_rule_extends_the_catalog_and_a_disable_removes_a_builtin() {
        let d = dir("extend");
        let catalog = load(&d, &[("vendor.toml", VENDOR)], &["alsa"]).unwrap();
        let ids: Vec<&str> = catalog.enabled().iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"vendor-framework"));
        assert!(!ids.contains(&"alsa"));
        assert!(ids.windows(2).all(|p| p[0] < p[1]), "{ids:?}");
        assert!(
            catalog.find("alsa").is_some(),
            "a disabled rule stays known"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn shadowing_a_builtin_is_refused_even_when_it_is_disabled() {
        let d = dir("shadow");
        let shadow = VENDOR.replace("vendor-framework", "alsa");
        let err = failure(load(&d, &[("alsa.toml", &shadow)], &["alsa"]));
        assert!(
            err.contains("alsa.toml") && err.contains("rule alsa shadows"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_repeated_local_identity_names_both_files() {
        let d = dir("repeat");
        let err = failure(load(&d, &[("a.toml", VENDOR), ("b.toml", VENDOR)], &[]));
        assert!(
            err.contains("b.toml") && err.contains("a.toml") && err.contains("vendor-framework"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn unknown_and_repeated_disables_are_refused() {
        let d = dir("disable");
        let err = failure(load(&d, &[], &["no-such-rule"]));
        assert!(
            err.contains("disable-rules") && err.contains("no-such-rule"),
            "{err}"
        );
        let err = failure(load(&d, &[], &["alsa", "alsa"]));
        assert!(
            err.contains("disable-rules") && err.contains("alsa"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bad_schema_unknown_fields_and_truncation_name_the_file() {
        let d = dir("schema");
        for (name, text) in [
            ("version.toml", VENDOR.replace("schema = 1", "schema = 2")),
            ("field.toml", VENDOR.replace("selection =", "selektion =")),
            ("cut.toml", VENDOR[..VENDOR.len() / 2].to_string()),
            (
                "ld.toml",
                VENDOR.replace("VENDOR_PLUGIN_DIR", "LD_LIBRARY_PATH"),
            ),
        ] {
            let err = failure(load(&d, &[(name, &text)], &[]));
            assert!(
                err.contains(name) && err.contains("[bundle] rules"),
                "{err}"
            );
        }
        let err = failure(load(&d, &[], &[]).and_then(|_| {
            RuleCatalog::load(
                &d.join("onelf.toml"),
                &d,
                &[PathBuf::from("absent.toml")],
                &[],
            )
        }));
        assert!(err.contains("absent.toml"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_conflicting_mapping_names_the_file_that_introduced_it() {
        let d = dir("conflict");
        let alias = VENDOR.replace("VENDOR_PLUGIN_DIR", "ALSA_PLUGIN_DIR");
        let err = failure(load(&d, &[("alias.toml", &alias)], &[]));
        assert!(
            err.contains("alias.toml") && err.contains("ALSA_PLUGIN_DIR"),
            "{err}"
        );
        assert!(
            load(&d, &[("alias.toml", &alias)], &["alsa"]).is_ok(),
            "disabling the built-in frees its variable"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The record is carried exactly when the fallback would not
    /// reproduce it: never for the built-ins alone or a disable of a rule
    /// without runtime behavior, always for a local runtime rule, a
    /// disabled built-in mapping, or a prepared pin.
    #[test]
    fn runtime_records_are_needed_only_where_the_fallback_differs() {
        let d = dir("records");
        let plain = RuleCatalog::default();
        assert_eq!(plain.runtime_records(&[]).unwrap(), None);
        let no_runtime = load(&d, &[], &["tcl-8.6"]).unwrap();
        assert_eq!(no_runtime.runtime_records(&[]).unwrap(), None);
        let silent = VENDOR[..VENDOR.find("[[runtime.paths]]").unwrap()].to_string();
        let discovery_only = load(&d, &[("silent.toml", &silent)], &[]).unwrap();
        assert_eq!(discovery_only.runtime_records(&[]).unwrap(), None);

        let local = load(&d, &[("vendor.toml", VENDOR)], &[]).unwrap();
        let records = local.runtime_records(&[]).unwrap().unwrap();
        assert!(records.iter().any(|r| r.id.as_str() == "vendor-framework"));
        assert!(records.iter().any(|r| r.id.as_str() == "alsa"));
        assert!(records.iter().all(|r| r.binding.is_none()));

        let disabled = load(&d, &[], &["alsa"]).unwrap();
        let records = disabled.runtime_records(&[]).unwrap().unwrap();
        assert!(!records.iter().any(|r| r.id.as_str() == "alsa"));
        assert!(records.iter().any(|r| r.id.as_str() == "gstreamer"));

        let pin = PreparedPin {
            set: "toolkit".to_string(),
            pin: "ab".repeat(32),
            root: PathBuf::from(".onelf-prepared/toolkit").join("ab".repeat(32)),
            files: Vec::new(),
            rules: vec!["gstreamer".to_string()],
        };
        let records = plain.runtime_records(&[pin]).unwrap().unwrap();
        let bound: Vec<_> = records.iter().filter(|r| r.binding.is_some()).collect();
        assert_eq!(bound.len(), 1);
        assert_eq!(bound[0].id.as_str(), "gstreamer");
        assert_eq!(bound[0].binding.as_ref().unwrap().name.as_str(), "toolkit");
        assert!(bound[0].runtime.hooks.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
}
